//! The slot of one child incarnation: the supervisor's control half, the
//! executor's half, and the one stop ladder the executor runs.
//!
//! A slot is a pair of `watch` cells. The supervisor writes the command cell
//! (a stop order) and reads the status cell; the incarnation's executor, its
//! runner or its restart attempt, reads the command cell and writes the
//! status cell. An order is level-triggered: it persists, so an executor
//! that starts after the order was written still honors it on its first
//! read. Every stop of a supervised child is escalated here, by the task
//! that owns the incarnation, so no caller future and no detached timer owns
//! any step of it: a caller that gives up waiting changes nothing about how
//! the child is stopped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::{oneshot, watch, Notify};
use tokio::task::{AbortHandle, JoinError, JoinHandle};
#[cfg(test)]
use tokio::time::timeout;
use tokio::time::{sleep_until, Instant};

use crate::actor::panic::payload_into_string;
use crate::actor::runtime::{bounded_wait, saturating_deadline};
use crate::actor::subtree::{AbortRecord, SubtreeWait, SubtreeWatch};
use crate::actor::supervision::rules::{
    attempt_step, first_signal, kill_grace_from, ladder_step, own_budget, successor_wait_step,
    AttemptAction, AttemptEvent, AttemptLadder, AttemptReport, AttemptStage, AttemptView,
    LadderAction, LadderEvent, LadderStep, TaskState, WaitAction,
};
use crate::actor::supervision::{ChildLinkGuard, RestartOutcome, DEFAULT_KILL_GRACE};
use crate::error::{ActorError, SpawnError};
use crate::system::read_status;
use crate::system::wait_end_or_floor;
use crate::types::{
    ActorId, ActorStatus, ChildFate, RaiseResult, Shutdown, StatusRx, StopLane, StopReason,
};

/// A stop decision written to one incarnation's executor. Level-triggered: it
/// persists, so an executor that starts after it was written still honors it
/// on its first read. `issued_at` is meaningful only when `initial_raised` is
/// true; otherwise the executor raises the first signal itself and the
/// policy clock starts at that raise.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StopOrder {
    pub policy: Shutdown,
    pub issued_at: Instant,
    pub initial_raised: bool,
}

/// What the supervisor tells the executor.
#[derive(Clone, Debug, Default)]
pub(crate) struct SlotCommand {
    pub stop: Option<StopOrder>,
    /// A vetoing `Infinity` occupant was asked once more by a group restart
    /// that joined its stop. Written by the supervisor only, through
    /// [`SlotCtl::mark_reasked`], never by the executor.
    pub reasked: bool,
    /// A group restart's evaluation cancels the restart attempt of a member
    /// whose actor was not yet spawned. Written by the supervisor only,
    /// through [`SlotCtl::cancel_unspawned`]; the executor honors it before
    /// its factory runs and while it waits for the previous incarnation's
    /// subtree, and ignores it once its incarnation was spawned.
    pub cancel: bool,
}

/// What bounds the stop of one incarnation after its supervisor ended: whether
/// the child is itself a supervisor, its own `Shutdown`, and its kill grace.
/// The waits on an incarnation whose runner failed read it through
/// [`own_budget`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OwnStop {
    pub is_supervisor: bool,
    pub shutdown: Shutdown,
    pub kill_grace: Duration,
}

impl OwnStop {
    /// A worker with the given kill grace.
    pub(crate) fn worker(kill_grace: Duration) -> Self {
        Self {
            is_supervisor: false,
            shutdown: Shutdown::default(),
            kill_grace,
        }
    }
}

impl Default for OwnStop {
    fn default() -> Self {
        Self::worker(DEFAULT_KILL_GRACE)
    }
}

/// What the executor tells the supervisor. `lane` and `status` are written
/// by the incarnation's executor right after the incarnation was spawned.
#[derive(Clone, Debug, Default)]
pub(crate) struct SlotCell {
    /// The incarnation's end, written once, after its task fully exited.
    pub fate: Option<ChildFate>,
    /// Set when the incarnation outlived its whole ladder, abort included.
    pub unresponsive: bool,
    /// Set by a restart attempt that honored a stop order before it called
    /// its factory: no incarnation existed.
    pub cancelled: bool,
    pub lane: Option<StopLane>,
    pub status: Option<StatusRx>,
    /// What the incarnation's stop is bounded by, published with its lane and
    /// status: the waits on an incarnation whose runner failed read it.
    pub own: Option<OwnStop>,
    /// The incarnation's subtree, published with its lane and status: the
    /// next incarnation of the child waits for it.
    pub subtree: Option<SubtreeWatch>,
}

/// The signal that wakes a supervisor's loop whenever an incarnation of one
/// of its slots receives the Unresponsive verdict.
#[derive(Default)]
pub(crate) struct Verdicts {
    notify: Notify,
    supervisor: OnceLock<ActorId>,
}

impl Verdicts {
    /// Names the supervisor the verdicts of its slots are logged for.
    pub(crate) fn set_supervisor(&self, supervisor: ActorId) {
        let _ = self.supervisor.set(supervisor);
    }

    /// Completes when a verdict was written since the last wake-up.
    pub(crate) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    pub(crate) fn notify_one(&self) {
        self.notify.notify_one();
    }
}

/// The flag of the Unresponsive verdict of one incarnation. One flag per
/// incarnation, shared by every writer of the verdict (its stop ladder, a
/// wait on its closed cell, and every wait for its subtree to end): the
/// first writer logs a warning for the verdict, naming the supervisor, the
/// child and the incarnation of the slot, and wakes the supervisor's loop;
/// any later writer finds the flag set and does neither.
#[derive(Clone)]
pub(crate) struct VerdictFlag {
    flag: Arc<AtomicBool>,
    verdicts: Arc<Verdicts>,
    key: Option<(ActorId, u64)>,
}

impl VerdictFlag {
    /// Records the verdict unless it was recorded already. Returns whether
    /// this call recorded it.
    pub(crate) fn set(&self) -> bool {
        self.record(LADDER_VERDICT)
    }

    /// Records the verdict of a wait for the incarnation's subtree that
    /// outlived its bound, unless the verdict was recorded already.
    pub(crate) fn set_for_subtree(&self) -> bool {
        self.record(SUBTREE_VERDICT)
    }

    fn record(&self, cause: &'static str) -> bool {
        let first = !self.flag.swap(true, Ordering::SeqCst);
        if first {
            log_verdict(self.verdicts.supervisor.get(), &self.key, cause);
            self.verdicts.notify_one();
        }
        first
    }

    pub(crate) fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// The supervisor, child and incarnation a log line about this slot's
    /// incarnation names, each `?` when unknown.
    #[cfg(feature = "tracing")]
    fn names(&self) -> (String, String, String) {
        let supervisor = self
            .verdicts
            .supervisor
            .get()
            .map_or("?".to_string(), |id| id.to_string());
        match &self.key {
            Some((child, incarnation)) => (supervisor, child.to_string(), incarnation.to_string()),
            None => (supervisor, "?".to_string(), "?".to_string()),
        }
    }
}

/// The warning of a verdict the stop ladder reached.
const LADDER_VERDICT: &str = "child outlived its whole stop ladder and is left behind unresponsive";

/// The warning of a verdict a wait for the incarnation's subtree reached.
const SUBTREE_VERDICT: &str =
    "child's subtree outlived the bound of the wait for it and is left behind unresponsive";

/// Logs a verdict once, where it is made. A slot without a key logs nothing.
#[allow(unused_variables)]
fn log_verdict(supervisor: Option<&ActorId>, key: &Option<(ActorId, u64)>, cause: &'static str) {
    #[cfg(feature = "tracing")]
    if let Some((child, incarnation)) = key {
        caught_log!(warn,
            supervisor = %supervisor.map_or("?".to_string(), |id| id.to_string()),
            child = %child,
            incarnation = *incarnation,
            "{cause}"
        );
    }
}

/// The subtree of one incarnation, with that incarnation's verdict flag: a
/// wait for the subtree that outlives its bound records the incarnation's
/// Unresponsive verdict through the flag its stop ladder writes, so one
/// incarnation's verdict logs one warning whichever route reaches it.
#[derive(Clone)]
pub(crate) struct Predecessor {
    subtree: SubtreeWatch,
    verdict: VerdictFlag,
}

impl Predecessor {
    /// True once every actor of the subtree has ended.
    pub(crate) fn is_gone(&self) -> bool {
        self.subtree.is_gone()
    }

    /// A predecessor whose verdict no supervisor reads and nothing logs.
    #[cfg(test)]
    pub(crate) fn untracked(subtree: SubtreeWatch) -> Self {
        Predecessor {
            subtree,
            verdict: VerdictFlag {
                flag: Arc::new(AtomicBool::new(false)),
                verdicts: Arc::default(),
                key: None,
            },
        }
    }
}

/// The supervisor's side of a slot: only the supervisor's own task writes
/// `cmd_tx` and reads `cell_rx`.
pub(crate) struct SlotCtl {
    cmd_tx: watch::Sender<SlotCommand>,
    cell_rx: watch::Receiver<SlotCell>,
    lost: VerdictFlag,
}

/// The executor's side of a slot. Deliberately not `Clone`: exactly one
/// executor owns it, and it moves only by an explicit hand-over.
pub(crate) struct SlotIo {
    cmd_rx: watch::Receiver<SlotCommand>,
    cell_tx: watch::Sender<SlotCell>,
    lost: VerdictFlag,
}

/// A slot's two halves. `verdicts` is the supervisor's signal, woken whenever
/// an incarnation of this slot receives the Unresponsive verdict. A slot made
/// without a key logs no verdict.
#[cfg(test)]
pub(crate) fn slot_pair(verdicts: Arc<Verdicts>) -> (SlotCtl, SlotIo) {
    slot_pair_for(verdicts, None)
}

/// As [`slot_pair`], for the incarnation `key` (the child and the token its
/// attempt and its runner work under): the verdict of the slot is logged under
/// that key.
pub(crate) fn slot_pair_for(
    verdicts: Arc<Verdicts>,
    key: Option<(ActorId, u64)>,
) -> (SlotCtl, SlotIo) {
    let (cmd_tx, cmd_rx) = watch::channel(SlotCommand::default());
    let (cell_tx, cell_rx) = watch::channel(SlotCell::default());
    let lost = VerdictFlag {
        flag: Arc::new(AtomicBool::new(false)),
        verdicts,
        key,
    };
    (
        SlotCtl {
            cmd_tx,
            cell_rx,
            lost: lost.clone(),
        },
        SlotIo {
            cmd_rx,
            cell_tx,
            lost,
        },
    )
}

/// The signal a manual stop's commit raises, purely a function of the
/// child's `Shutdown` policy: what the stop was asked for decides what
/// happens once the real fate is observed, not what gets signaled.
pub(crate) fn initial_signal(shutdown: Shutdown) -> StopReason {
    first_signal(shutdown).reason()
}

fn vetoable(reason: &StopReason) -> bool {
    matches!(reason, StopReason::Graceful | StopReason::ParentRequest)
}

impl SlotIo {
    /// The reason the published incarnation's status plane carries, if one
    /// is written.
    fn plane_reason(&self) -> Option<StopReason> {
        self.cell_tx
            .borrow()
            .status
            .as_ref()
            .and_then(|status| status.borrow().reason.clone())
    }

    /// Records the incarnation's end. The verdict flag stays as it is: an
    /// incarnation that was flagged and then ended still has its fate.
    pub(crate) fn record_fate(&self, fate: ChildFate) {
        self.cell_tx.send_modify(|cell| cell.fate = Some(fate));
    }

    /// Publishes the incarnation's stop lane, status plane, stop bounds and
    /// subtree into the cell, together.
    pub(crate) fn publish(
        &self,
        lane: StopLane,
        status: StatusRx,
        own: OwnStop,
        subtree: Option<SubtreeWatch>,
    ) {
        self.cell_tx.send_modify(|cell| {
            cell.lane = Some(lane);
            cell.status = Some(status);
            cell.own = Some(own);
            cell.subtree = subtree;
        });
    }

    /// Records that the incarnation outlived its whole ladder, and wakes the
    /// supervisor's loop so it can re-check what waits on the incarnation.
    pub(crate) fn write_verdict(&self) {
        // The shared flag first: it logs the warning, so no waiter can see the
        // verdict published before the warning exists.
        self.lost.set();
        self.cell_tx.send_modify(|cell| cell.unresponsive = true);
    }

    /// Records that the attempt ended without an incarnation: a stop order
    /// or the group's cancel was honored before the attempt's factory ran,
    /// or the attempt found its child's spec removed, before or during its
    /// build.
    pub(crate) fn mark_cancelled(&self) {
        self.cell_tx.send_modify(|cell| cell.cancelled = true);
    }

    /// The order written for the incarnation, if any.
    pub(crate) fn order(&self) -> Option<StopOrder> {
        self.cmd_rx.borrow().stop
    }

    /// Whether the command cell holds a stop order or the cancel flag, read
    /// as new: the next `changed()` waits for a later write. The two stages
    /// at which an attempt has not spawned its actor, the wait for the
    /// previous incarnation's subtree and the check before the factory, read
    /// it and nothing else.
    pub(crate) fn cancel_or_order(&mut self) -> bool {
        let command = self.cmd_rx.borrow_and_update();
        command.stop.is_some() || command.cancel
    }
}

impl SlotCtl {
    /// The incarnation's stop lane, once its executor published it.
    pub(crate) fn lane(&self) -> Option<StopLane> {
        self.cell_rx.borrow().lane.clone()
    }

    /// The incarnation's status plane, once its executor published it.
    pub(crate) fn status(&self) -> Option<StatusRx> {
        self.cell_rx.borrow().status.clone()
    }

    /// The incarnation's subtree, once its executor published it.
    pub(crate) fn subtree(&self) -> Option<SubtreeWatch> {
        self.cell_rx.borrow().subtree.clone()
    }

    /// `subtree`, as the subtree of this slot's incarnation.
    pub(crate) fn predecessor(&self, subtree: SubtreeWatch) -> Predecessor {
        Predecessor {
            subtree,
            verdict: self.lost.clone(),
        }
    }

    /// The subtree the cell holds, as the subtree of this slot's
    /// incarnation, when the cell holds one.
    pub(crate) fn recorded_predecessor(&self) -> Option<Predecessor> {
        self.subtree().map(|subtree| self.predecessor(subtree))
    }

    /// A handle to wait on the incarnation's end: the status cell and the
    /// incarnation's verdict flag.
    pub(crate) fn watch(&self) -> CellWatch {
        CellWatch {
            cell: self.cell_rx.clone(),
            lost: self.lost.clone(),
        }
    }

    /// A clone of the status cell, for tests that read it directly.
    #[cfg(test)]
    pub(crate) fn cell(&self) -> watch::Receiver<SlotCell> {
        self.cell_rx.clone()
    }

    /// Whether the occupant's task published its end: the status plane the
    /// cell holds reads `Stopped` or its sender is gone, whether the cell is
    /// open or not. False when the cell is open and no status was published
    /// yet.
    pub(crate) fn status_ended(&self) -> bool {
        match self.status() {
            Some(status) => {
                status.borrow().status == ActorStatus::Stopped || status.has_changed().is_err()
            }
            None => self.cell_rx.has_changed().is_err(),
        }
    }

    /// Records, on the supervisor's side of the slot, that a group restart
    /// asked the occupant once more.
    pub(crate) fn mark_reasked(&self) {
        self.cmd_tx.send_if_modified(|command| {
            let first = !command.reasked;
            command.reasked = true;
            first
        });
    }

    /// Whether a group restart asked the occupant once more.
    pub(crate) fn reasked(&self) -> bool {
        self.cmd_tx.borrow().reasked
    }

    /// The incarnation's recorded end, if its runner wrote one.
    pub(crate) fn fate(&self) -> Option<ChildFate> {
        self.cell_rx.borrow().fate.clone()
    }

    /// True once the incarnation received the Unresponsive verdict: its
    /// executor wrote it, or a wait on its closed cell recorded it.
    pub(crate) fn is_unresponsive(&self) -> bool {
        self.cell_rx.borrow().unresponsive || self.lost.is_set()
    }

    /// True when a stop order was already written.
    pub(crate) fn has_order(&self) -> bool {
        self.cmd_tx.borrow().stop.is_some()
    }

    /// True when the incarnation's lane holds a request its `pre_stop` may
    /// still veto (`Graceful` or `ParentRequest`).
    pub(crate) fn lane_vetoable(&self) -> bool {
        self.lane()
            .and_then(|lane| lane.reason())
            .is_some_and(|reason| vetoable(&reason))
    }

    /// Whether the supervisor can see that the incarnation's task ended: its
    /// fate is recorded, or the cell is closed and the status plane it held
    /// reads `Stopped` or closed (a lost runner's ended task). A closed cell
    /// whose incarnation still runs is `RunnerLost`.
    pub(crate) fn task_state(&self) -> TaskState {
        if self.cell_rx.borrow().fate.is_some() {
            return TaskState::Ended;
        }
        if self.cell_rx.has_changed().is_ok() {
            return TaskState::Live;
        }
        match self.status() {
            Some(status) => {
                let stopped = status.borrow().status == ActorStatus::Stopped;
                if stopped || status.has_changed().is_err() {
                    TaskState::Ended
                } else {
                    TaskState::RunnerLost
                }
            }
            None => TaskState::Ended,
        }
    }

    /// How an ended task ended: the fate's reason, or for a closed cell the
    /// reason the incarnation's status plane holds.
    pub(crate) fn end_reason(&self) -> StopReason {
        self.fate().map_or_else(
            || {
                self.status()
                    .map_or(StopReason::Kill, |status| status.borrow().reason_or_kill())
            },
            |fate| fate.reason,
        )
    }

    /// Raises the first signal of a stop on the incarnation's lane. Returns
    /// whether it was raised: nothing is raised when no lane was published
    /// yet. A stop that joins one in progress never calls this: a second
    /// signal of the same strength would fire the child's `pre_stop` again.
    pub(crate) fn raise_first(&self, policy: Shutdown) -> bool {
        match self.lane() {
            Some(lane) => {
                // A `Kill` raised here (a child whose shutdown policy is
                // `Kill`) belongs to the supervisor's ladder, which holds the
                // child's abort itself: no registry deadline is declared.
                lane.raise(initial_signal(policy));
                true
            }
            None => false,
        }
    }

    /// Writes the stop order for the incarnation. The first order wins and
    /// keeps its issue time: a second origin never restarts the clock.
    pub(crate) fn write_order(&self, policy: Shutdown, initial_raised: bool) {
        let order = StopOrder {
            policy,
            issued_at: Instant::now(),
            initial_raised,
        };
        self.cmd_tx.send_if_modified(|command| {
            if command.stop.is_none() {
                command.stop = Some(order);
                true
            } else {
                false
            }
        });
    }

    /// The stop of one incarnation by any origin: when `raise_first` and the
    /// cell holds a lane, raises the first signal and writes the order as
    /// already raised; otherwise writes it unraised, so the executor raises
    /// the first signal when it can (an attempt not yet spawned). Only the
    /// first order is written.
    pub(crate) fn order_stop(&self, policy: Shutdown, raise_first: bool) {
        if self.has_order() {
            return;
        }
        let raised = raise_first && self.raise_first(policy);
        self.write_order(policy, raised);
    }

    /// Cancels the attempt of this slot if its actor was not spawned yet: it
    /// writes the flag and awaits nothing. An attempt that is already past its
    /// check before the factory ignores it.
    pub(crate) fn cancel_unspawned(&self) {
        self.cmd_tx.send_if_modified(|command| {
            let first = !command.cancel;
            command.cancel = true;
            first
        });
    }

    /// Whether the cancel flag was written.
    #[cfg(test)]
    pub(crate) fn cancel_written(&self) -> bool {
        self.cmd_tx.borrow().cancel
    }

    /// One more request on a vetoing child's lane: re-fires its `pre_stop`.
    pub(crate) fn ask_again(&self) {
        if let Some(lane) = self.lane() {
            lane.raise(StopReason::ParentRequest);
        }
    }

    /// The part of a lost runner's incarnation that outlives the runner: its
    /// status plane. `None` when no incarnation was ever spawned.
    pub(crate) fn lost_end(&self) -> Option<LostEnd> {
        let cell = self.cell_rx.borrow();
        let status = cell.status.clone()?;
        Some(LostEnd {
            status,
            lost: self.lost.clone(),
            own: cell.own.unwrap_or_default(),
            record: cell.lane.as_ref().map(|lane| lane.abort_record().clone()),
        })
    }
}

/// The end of an incarnation whose runner failed, read from the status plane
/// its cell held.
pub(crate) struct LostEnd {
    status: StatusRx,
    lost: VerdictFlag,
    /// What bounds the incarnation's own stop.
    own: OwnStop,
    /// The incarnation's abort record: the verdict waits for the abort that
    /// could end it.
    record: Option<AbortRecord>,
}

/// How long a wait on an incarnation whose runner failed lasts before the
/// incarnation is called unresponsive: for a worker, the guard's `Kill` takes
/// the incarnation's kill grace to leave the loop and the abort the settle
/// time to end a task that yields; a supervisor child stops in order first,
/// within its own `Shutdown` window, and has no bound at all under
/// `Shutdown::Infinity`. The verdict also waits for the abort issued against
/// the incarnation, as [`LostEnd::wait`] says.
fn lost_end_bound(own: OwnStop) -> Duration {
    bounded_wait(
        own_budget(own.is_supervisor, own.shutdown, own.kill_grace).unwrap_or(Duration::MAX),
    )
}

impl LostEnd {
    /// Waits until the incarnation's task ended (status `Stopped`, or the
    /// status sender dropped) and returns the reason it ended with, the one
    /// its status plane carries. The guard that fires together with a failed
    /// runner raises the incarnation's inherited stop on its lane and feeds
    /// the reaper: a worker still running then ends with `Kill`, whether its
    /// task obeyed the request or was aborted, and a supervisor child ends
    /// with the reason it chose, `ParentKilled` unless an earlier reason
    /// outranks it, or with `Kill` when the reaper's `Kill` or the abort ended
    /// it; either can read `Failure(Panic)` when its task unwound outside
    /// every catch. An incarnation that had ended before the guard fired keeps
    /// its own reason. An incarnation that outlives the bound of its stop
    /// and the abort issued against it by the settle time receives the
    /// Unresponsive verdict, recorded unless a waiting caller recorded it
    /// first, and the wait goes on; an abort still owed holds the verdict for
    /// at most the bound once more plus one more settle time
    /// ([`verdict_floor`](crate::actor::supervision::rules::verdict_floor)).
    pub(crate) async fn wait(mut self) -> StopReason {
        let bound = lost_end_bound(self.own);
        let ended = wait_end_or_floor(&mut self.status, self.record.as_ref(), bound, bound).await;
        if !ended {
            self.lost.set();
            let _ = self
                .status
                .wait_for(|value| value.status == ActorStatus::Stopped)
                .await;
        }
        if let Some(record) = &self.record {
            record.task_ended();
        }
        self.status.borrow().reason_or_kill()
    }
}

/// What a wait on a slot's cell holds: the status cell, and the
/// incarnation's verdict flag.
pub(crate) struct CellWatch {
    cell: watch::Receiver<SlotCell>,
    lost: VerdictFlag,
}

/// How a wait on a slot's cell ended.
#[derive(Debug)]
pub(crate) enum CellOutcome {
    /// The incarnation ended, with this recorded or derived fate.
    Fate(ChildFate),
    /// The incarnation outlived its whole ladder.
    Unresponsive,
    /// The attempt honored a stop order before its factory ran.
    Cancelled,
    /// The executor failed before any incarnation existed.
    NoIncarnation,
}

/// Waits until the cell settles: a fate, the Unresponsive verdict, or a
/// cancel. A fate wins over the flag. A cell its executor dropped without
/// settling (the executor's task failed) settles nothing by itself: the wait
/// then follows the status plane the cell last held until the incarnation's
/// task ended, bounded by the incarnation's own stop budget, then gives the
/// verdict and records it (unless the replacement task of the failed runner
/// recorded it first). A cancel the executor wrote is read from the last
/// value the cell held, so a sender dropped right after it still reads as a
/// cancel.
pub(crate) async fn await_cell(watch: &mut CellWatch, incarnation: u64) -> CellOutcome {
    let settled = match watch
        .cell
        .wait_for(|cell| cell.fate.is_some() || cell.unresponsive || cell.cancelled)
        .await
    {
        Ok(cell) => Some(if let Some(fate) = &cell.fate {
            CellOutcome::Fate(fate.clone())
        } else if cell.unresponsive {
            CellOutcome::Unresponsive
        } else {
            CellOutcome::Cancelled
        }),
        Err(_) => None,
    };
    match settled {
        Some(outcome) => outcome,
        None => closed_cell(watch, incarnation).await,
    }
}

/// The wait on a cell whose executor is gone. The last value the cell held
/// says where the incarnation's status plane is.
async fn closed_cell(watch: &CellWatch, incarnation: u64) -> CellOutcome {
    let last = watch.cell.borrow().clone();
    let Some(mut status) = last.status else {
        return CellOutcome::NoIncarnation;
    };
    // A plane closed without `Stopped` reads as ended, as every reader of
    // the plane reads it: its task ended and nothing is left to publish.
    let ended_now = read_status(&status) == ActorStatus::Stopped;
    if !ended_now && watch.lost.is_set() {
        return CellOutcome::Unresponsive;
    }
    let bound = lost_end_bound(last.own.unwrap_or_default());
    let record = last.lane.as_ref().map(StopLane::abort_record);
    let ended = wait_end_or_floor(&mut status, record, bound, bound).await;
    if ended {
        if let Some(record) = record {
            record.task_ended();
        }
        // The reason the plane carries: the one written at the incarnation's
        // seal, or `Kill` for a plane closed without one.
        let reason = status.borrow().reason_or_kill();
        CellOutcome::Fate(ChildFate {
            reason,
            incarnation,
        })
    } else {
        watch.lost.set();
        CellOutcome::Unresponsive
    }
}

/// The panic of a task that failed, as the error the supervisor records:
/// `None` for a task that was cancelled.
pub(crate) fn join_error_panic(err: JoinError) -> Option<ActorError> {
    if !err.is_panic() {
        return None;
    }
    Some(match err.try_into_panic() {
        Ok(payload) => ActorError::Panic(payload_into_string(payload)),
        Err(_) => ActorError::Panic("panic payload unavailable".to_string()),
    })
}

/// The stop reason of an incarnation whose task the runner or the attempt
/// joined. A task that panicked wrote its reason into its status plane during
/// its own unwind, before the join resolved: that reason is the end's, so the
/// plane and the report carry one reason, and the join's payload is logged.
/// `plane` is called only for a task that panicked. Any other end reads as
/// [`map_join`] reads it. The log line names the
/// supervisor, the child and the incarnation of `io`'s slot, `?` where one
/// is unknown (no slot, or a slot made without a key).
#[allow(unused_variables)]
pub(crate) fn joined_reason(
    res: Result<StopReason, JoinError>,
    plane: impl FnOnce() -> Option<StopReason>,
    io: Option<&SlotIo>,
) -> StopReason {
    match res {
        Err(err) if err.is_panic() => match plane() {
            Some(reason) => {
                #[cfg(feature = "tracing")]
                {
                    let (supervisor, child, incarnation) = io.map_or_else(
                        || ("?".to_string(), "?".to_string(), "?".to_string()),
                        |io| io.lost.names(),
                    );
                    caught_log!(warn,
                        supervisor = %supervisor,
                        child = %child,
                        incarnation = %incarnation,
                        reason = %reason,
                        panic = %map_join(Err(err)),
                        "an actor's task panicked outside every catch"
                    );
                }
                #[cfg(not(feature = "tracing"))]
                drop(err);
                reason
            }
            None => map_join(Err(err)),
        },
        res => map_join(res),
    }
}

/// Turns the result of awaiting an incarnation's task into its stop reason.
pub(crate) fn map_join(res: Result<StopReason, JoinError>) -> StopReason {
    match res {
        Ok(reason) => reason,
        Err(err) if err.is_panic() => match err.try_into_panic() {
            Ok(payload) => StopReason::Failure(ActorError::Panic(payload_into_string(payload))),
            Err(_) => {
                StopReason::Failure(ActorError::Panic("panic payload unavailable".to_string()))
            }
        },
        // The only other `JoinError` case is `is_cancelled()`. This crate
        // never hands a raw `AbortHandle` to user code, so an abort here
        // can only be this crate's own escalation (the supervision
        // ladder, the system shutdown ladder, `ActorSystem::kill_by_id`,
        // or `ActorHandle::stop_within`) - this is the single place that
        // turns an abort into a reason, and it reports it truthfully as
        // `Kill`.
        Err(_) => StopReason::Kill,
    }
}

/// Raises a ladder's request; a `Kill` the lane takes makes the ladder's
/// abort owed to the incarnation until it is issued or the task ends. A task
/// that already ended is owed nothing.
fn raise_owing(lane: &StopLane, reason: StopReason, task: &AbortHandle) {
    let kill = matches!(reason, StopReason::Kill);
    // The supervisor holds the child's abort handle and record itself, so no
    // registry entry is involved and no deadline is declared.
    if lane.try_raise(reason) == RaiseResult::Recorded && kill {
        lane.abort_record().owe_for(task);
    }
}

/// An attempt's ladder enters its kill grace at a start timeout or an ended
/// initialization: it raises `Kill` when `raise` says the lane holds none
/// yet, and owes its abort either way, since the ladder aborts the task once
/// the grace has passed. A task that already ended is owed nothing.
fn owe_kill_grace(lane: &StopLane, raise: bool, task: &AbortHandle) {
    if raise {
        raise_owing(lane, StopReason::Kill, task);
    } else {
        lane.abort_record().owe_for(task);
    }
}

/// Runs one step of the ladder: asks the pure decision, then performs the
/// action it names.
fn advance(
    step: LadderStep,
    event: LadderEvent,
    grace: Duration,
    join: &JoinHandle<StopReason>,
    lane: &StopLane,
    io: &SlotIo,
) -> LadderStep {
    let (next, action) = ladder_step(step, event, Instant::now().into_std(), grace);
    match action {
        LadderAction::None => {}
        LadderAction::Raise(signal) => raise_owing(lane, signal.reason(), &join.abort_handle()),
        LadderAction::Abort => lane.abort_record().abort(&join.abort_handle()),
        LadderAction::WriteVerdict => io.write_verdict(),
        LadderAction::ReleaseMark => lane.abort_record().task_ended(),
    }
    next
}

/// The one stop ladder of an incarnation: runs while the incarnation's task
/// runs, honoring the stop order in the command cell, and returns the
/// task's stop reason once it ended. `grace` is the incarnation's kill
/// grace: the time between its `Kill` and its abort.
///
/// A graceful window runs under the order's policy; at its end the ladder
/// raises `Kill`, waits `grace`, aborts the task, waits the settle time, and
/// then writes the Unresponsive verdict, after which it keeps waiting for
/// the task.
pub(crate) async fn run_ladder(
    join: &mut JoinHandle<StopReason>,
    lane: &StopLane,
    io: &mut SlotIo,
    grace: Duration,
) -> StopReason {
    let mut step = LadderStep::Idle;
    let mut commands_open = true;

    // Level-triggered read at start: an order written before this ladder
    // existed is honored at once.
    let first = io.cmd_rx.borrow_and_update().stop;
    if let Some(order) = first {
        step = advance(step, order_event(order), grace, join, lane, io);
    }

    loop {
        let deadline = step.deadline();
        let armed = !matches!(step, LadderStep::Idle);
        tokio::select! {
            biased;

            res = &mut *join => {
                let reason = joined_reason(res, || io.plane_reason(), Some(io));
                advance(step, LadderEvent::TaskEnded, grace, join, lane, io);
                return reason;
            }

            changed = io.cmd_rx.changed(), if commands_open && !armed => {
                match changed {
                    Ok(()) => {
                        let order = io.cmd_rx.borrow_and_update().stop;
                        if let Some(order) = order {
                            step = advance(step, order_event(order), grace, join, lane, io);
                        }
                    }
                    // The supervision state is gone: the link guard owns the
                    // teardown from here.
                    Err(_) => commands_open = false,
                }
            }

            _ = sleep_until_deadline(deadline), if deadline.is_some() => {
                step = advance(step, LadderEvent::DeadlineReached, grace, join, lane, io);
            }
        }
    }
}

/// The attempt's check of its command cell before it calls its factory: an
/// order or the cancel flag present cancels the attempt (no incarnation
/// exists, and nothing is built). Returns whether the attempt was cancelled;
/// the cancel is recorded in the cell for the wait on it.
pub(crate) fn cancelled_before_factory(io: &mut SlotIo) -> bool {
    let view = AttemptView {
        ladder: AttemptLadder::Off,
        kill_stored: false,
        acked: false,
    };
    let begin = AttemptEvent::Begin {
        ordered: io.cancel_or_order(),
    };
    let cancel = attempt_step(AttemptStage::Factory, begin, view) == AttemptAction::Cancel;
    if cancel {
        io.mark_cancelled();
    }
    cancel
}

/// A restart attempt's wait, before its factory, for the previous
/// incarnation's whole subtree to end. An attempt without a previous
/// incarnation to wait for goes on at once.
///
/// The wait is bounded by the budget the subtree carries (unbounded only
/// while an unbounded supervisor child lives below it), honors a stop order
/// or the cancel flag at every moment, and returns the decision of
/// [`successor_wait_step`]: `Proceed`, `Cancel` or `Escalate`. A cancel is
/// recorded in the cell, so a wait on the cell reads it; an `Escalate`
/// records the previous incarnation's Unresponsive verdict.
pub(crate) async fn await_predecessor(
    predecessor: Option<&Predecessor>,
    io: &mut SlotIo,
) -> WaitAction {
    let Some(predecessor) = predecessor else {
        return WaitAction::Proceed;
    };
    let mut wait = SubtreeWait::new(&predecessor.subtree);
    let mut commands_open = true;
    loop {
        // Level-triggered: an order or the cancel flag written before the
        // wait began, or while it ran, is read here.
        let ordered = io.cancel_or_order();
        match successor_wait_step(wait.gone(), ordered, wait.expired()) {
            WaitAction::Keep => {}
            decided => {
                match decided {
                    WaitAction::Cancel => io.mark_cancelled(),
                    WaitAction::Escalate => {
                        predecessor.verdict.set_for_subtree();
                    }
                    WaitAction::Proceed | WaitAction::Keep => {}
                }
                return decided;
            }
        }
        tokio::select! {
            biased;

            changed = io.cmd_rx.changed(), if commands_open => {
                // The supervision state is gone: the link guard owns the
                // teardown from here.
                if changed.is_err() {
                    commands_open = false;
                }
            }

            () = wait.changed() => {}
        }
    }
}

/// A handler's wait for a subtree to end: the caller of `terminate_child`
/// or `spawn_child` waits for the actors a previous incarnation spawned. No
/// order exists here, so the answers are `Proceed` and `Escalate`; the
/// bound is the attempt's. An `Escalate` records the previous incarnation's
/// Unresponsive verdict.
pub(crate) async fn await_subtree(predecessor: &Predecessor) -> WaitAction {
    let mut wait = SubtreeWait::new(&predecessor.subtree);
    loop {
        match successor_wait_step(wait.gone(), false, wait.expired()) {
            WaitAction::Keep => wait.changed().await,
            WaitAction::Escalate => {
                predecessor.verdict.set_for_subtree();
                return WaitAction::Escalate;
            }
            decided => return decided,
        }
    }
}

#[cfg(test)]
mod lost_end_cap_tests {
    use super::*;
    use crate::actor::runtime::FAR_FUTURE;
    use crate::actor::supervision::ABORT_SETTLE;
    use crate::types::{StatusTx, StatusValue};

    #[test]
    fn the_lost_end_bound_is_the_kill_grace_plus_the_settle_time() {
        let grace = Duration::from_millis(300);
        assert_eq!(lost_end_bound(OwnStop::worker(grace)), grace + ABORT_SETTLE);
        assert_eq!(lost_end_bound(OwnStop::worker(Duration::MAX)), FAR_FUTURE);
    }

    /// A wait on a cell whose executor is gone follows the status plane, and
    /// the fate it derives carries the reason the plane holds: an incarnation
    /// that ended `Graceful` reads `Graceful`.
    async fn a_closed_cell_reads_the_reason_its_plane_carries() {
        let (ctl, io) = slot_pair(Default::default());
        let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _stop_rx) = StopLane::new();
        io.publish(
            lane,
            status_rx,
            OwnStop::worker(Duration::from_secs(5)),
            None,
        );
        let mut watch = ctl.watch();
        drop(io);
        StatusValue::write_reason(&status_tx, StopReason::Graceful, true);
        let outcome = tokio::time::timeout(Duration::from_secs(5), await_cell(&mut watch, 1))
            .await
            .expect("the wait ends once the plane reads Stopped");
        match outcome {
            CellOutcome::Fate(fate) => assert_eq!(fate.reason, StopReason::Graceful),
            other => panic!("expected the incarnation's fate, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_closed_cell_reads_the_reason_its_plane_carries_current_thread() {
        a_closed_cell_reads_the_reason_its_plane_carries().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_cell_reads_the_reason_its_plane_carries_multi_thread() {
        a_closed_cell_reads_the_reason_its_plane_carries().await;
    }

    #[tokio::test(start_paused = true)]
    async fn lost_runner_waits_cap_a_saturated_kill_grace() {
        let year = Duration::from_secs(86_400 * 365);
        let (ctl, io) = slot_pair(Default::default());
        let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _stop_rx) = StopLane::new();
        io.publish(lane, status_rx, OwnStop::worker(Duration::MAX), None);
        let mut watch = ctl.watch();
        let lost = ctl.lost_end().expect("a status was published");
        // The executor is gone: both waits follow the status plane, which
        // never reads `Stopped`.
        drop(io);
        let closed = tokio::spawn(async move { await_cell(&mut watch, 1).await });
        let end = tokio::spawn(lost.wait());
        tokio::time::advance(year * 29).await;
        tokio::task::yield_now().await;
        assert!(
            !closed.is_finished(),
            "a saturated grace never reads as short"
        );
        assert!(!ctl.is_unresponsive());
        tokio::time::advance(year * 2).await;
        let outcome = closed.await.expect("the wait returns, it does not panic");
        assert!(matches!(outcome, CellOutcome::Unresponsive), "{outcome:?}");
        assert!(ctl.is_unresponsive());
        // The wait of a lost runner goes on after the verdict, and ends with
        // the reason the incarnation's plane carries once it reports
        // `Stopped`.
        StatusValue::write_reason(&status_tx, StopReason::Graceful, true);
        let reason = tokio::time::timeout(Duration::from_secs(1), end)
            .await
            .expect("the wait ends once the incarnation stopped")
            .expect("the wait does not panic");
        assert_eq!(reason, StopReason::Graceful);
    }
}

/// Everything a restart attempt holds from the moment its incarnation was
/// spawned until it hands the incarnation over or reports its end.
pub(crate) struct Attempt {
    pub child_id: ActorId,
    pub incarnation: u64,
    pub join: JoinHandle<StopReason>,
    pub lane: StopLane,
    /// Armed since the spawn: if the attempt is dropped before it returns,
    /// the guard's ladder ends the incarnation.
    pub guard: ChildLinkGuard,
    pub ack: oneshot::Receiver<Result<(), ActorError>>,
    pub start_timeout: Option<Duration>,
    /// The incarnation's kill grace.
    pub grace: Duration,
    pub io: SlotIo,
}

/// One step of an attempt's ladder: asks the pure ladder, then performs the
/// action it names. The verdict is written only when the attempt's own
/// decision procedure says so.
fn advance_attempt(
    step: LadderStep,
    event: LadderEvent,
    grace: Duration,
    attempt: (&JoinHandle<StopReason>, &StopLane, &SlotIo),
) -> LadderStep {
    let (join, lane, io) = attempt;
    let (next, action) = ladder_step(step, event, Instant::now().into_std(), grace);
    match action {
        LadderAction::None => {}
        LadderAction::Raise(signal) => raise_owing(lane, signal.reason(), &join.abort_handle()),
        LadderAction::Abort => lane.abort_record().abort(&join.abort_handle()),
        LadderAction::WriteVerdict => {
            let view = AttemptView {
                ladder: step.position(),
                kill_stored: false,
                acked: true,
            };
            if attempt_step(AttemptStage::Init, AttemptEvent::LadderSpent, view)
                == AttemptAction::WriteVerdict
            {
                io.write_verdict();
            }
        }
        LadderAction::ReleaseMark => lane.abort_record().task_ended(),
    }
    next
}

/// The executor of a restart attempt from the spawn of its incarnation: it
/// waits for the init acknowledgment, bounded by the start timeout from the
/// spawn, and honors a stop order at every moment of that wait. An order
/// starts the one ladder at once (its window from the order, the start
/// timeout still running, whichever ends first), and when the ladder ends
/// the attempt records the fate and reports the incarnation as stopped. An
/// acknowledgment read under no order hands the incarnation over, even when
/// its task already ended. An init error, a closed acknowledgment channel
/// and a start timeout wait for the task to end before any fate is written;
/// a task still running after the ladder's two waits receives the
/// Unresponsive verdict instead, and the attempt keeps waiting for it.
pub(crate) async fn run_attempt(attempt: Attempt) -> RestartOutcome {
    let Attempt {
        child_id,
        incarnation,
        mut join,
        lane,
        mut guard,
        ack,
        start_timeout,
        grace,
        mut io,
    } = attempt;
    let mut ack = Some(ack);
    let mut step = LadderStep::Idle;
    let mut ordered = false;
    let mut acked = false;
    let mut commands_open = true;
    let mut timeout_raised = false;
    let start_deadline = start_timeout.map(|dur| saturating_deadline(Instant::now(), dur));
    let mut timer_pending = start_deadline.is_some();

    macro_rules! view {
        () => {
            AttemptView {
                ladder: step.position(),
                kill_stored: matches!(lane.reason(), Some(StopReason::Kill)),
                acked,
            }
        };
    }
    // An order is read: the ladder starts from it.
    macro_rules! read_order {
        ($order:expr) => {{
            ordered = true;
            if attempt_step(AttemptStage::Init, AttemptEvent::Order, view!())
                == AttemptAction::ArmLadder
            {
                step = advance_attempt(step, order_event($order), grace, (&join, &lane, &io));
            }
        }};
    }

    // Level-triggered read at start: an order written before this executor
    // got here is honored at once.
    let first = io.cmd_rx.borrow_and_update().stop;
    if let Some(order) = first {
        read_order!(order);
    }

    loop {
        let deadline = step.deadline();
        tokio::select! {
            biased;

            changed = io.cmd_rx.changed(), if commands_open && !ordered => {
                match changed {
                    Ok(()) => {
                        let order = io.cmd_rx.borrow_and_update().stop;
                        if let Some(order) = order {
                            read_order!(order);
                        }
                    }
                    // The supervision state is gone: the link guard owns the
                    // teardown from here.
                    Err(_) => commands_open = false,
                }
            }

            result = async { ack.as_mut().expect("guarded by is_some").await }, if ack.is_some() => {
                ack = None;
                match result {
                    Ok(Ok(())) => {
                        acked = true;
                        // The command cell is read once more at the
                        // acknowledgment: an order not yet read is honored
                        // before the incarnation could be handed over.
                        if !ordered {
                            let order = io.cmd_rx.borrow_and_update().stop;
                            if let Some(order) = order {
                                read_order!(order);
                            }
                        }
                        let event = AttemptEvent::Ack {
                            ordered: ordered || io.order().is_some(),
                            ended: join.is_finished(),
                        };
                        if attempt_step(AttemptStage::Init, event, view!()) == AttemptAction::Adopt {
                            return RestartOutcome::Adopted {
                                child_id,
                                incarnation,
                                new_stop_lane: lane,
                                new_join: join,
                                guard,
                                io,
                            };
                        }
                    }
                    Ok(Err(_)) | Err(_) => {
                        if let AttemptAction::KillPath { raise } =
                            attempt_step(AttemptStage::Init, AttemptEvent::InitEnded, view!())
                        {
                            owe_kill_grace(&lane, raise, &join.abort_handle());
                            step = kill_grace_from(Instant::now().into_std(), grace);
                        }
                    }
                }
            }

            res = &mut join => {
                let reason = joined_reason(res, || io.plane_reason(), Some(&io));
                advance_attempt(step, LadderEvent::TaskEnded, grace, (&join, &lane, &io));
                let event = AttemptEvent::Ended {
                    ordered: ordered || io.order().is_some(),
                    timeout_raised,
                };
                let report = match attempt_step(AttemptStage::Init, event, view!()) {
                    AttemptAction::Report(report) => report,
                    _ => AttemptReport::Failed,
                };
                io.record_fate(ChildFate {
                    reason: reason.clone(),
                    incarnation,
                });
                // The incarnation ended and its fate is recorded: the guard
                // has nothing left to force.
                guard.disarm();
                return match report {
                    AttemptReport::Stopped => RestartOutcome::Stopped {
                        child_id,
                        incarnation,
                        reason,
                    },
                    AttemptReport::StartTimeout => RestartOutcome::Failed {
                        child_id,
                        incarnation,
                        reason: StopReason::Failure(ActorError::Spawn(SpawnError::StartTimeout)),
                    },
                    AttemptReport::Failed => RestartOutcome::Failed {
                        child_id,
                        incarnation,
                        reason,
                    },
                };
            }

            _ = sleep_until_deadline(deadline), if deadline.is_some() => {
                step = advance_attempt(
                    step,
                    LadderEvent::DeadlineReached,
                    grace,
                    (&join, &lane, &io),
                );
            }

            _ = sleep_until(start_deadline.unwrap_or_else(Instant::now)), if timer_pending => {
                timer_pending = false;
                if let AttemptAction::KillPath { raise } =
                    attempt_step(AttemptStage::Init, AttemptEvent::StartTimeout, view!())
                {
                    timeout_raised = raise;
                    owe_kill_grace(&lane, raise, &join.abort_handle());
                    step = kill_grace_from(Instant::now().into_std(), grace);
                }
            }
        }
    }
}

fn order_event(order: StopOrder) -> LadderEvent {
    LadderEvent::Order {
        policy: order.policy,
        issued_at: order.issued_at.into_std(),
        initial_raised: order.initial_raised,
    }
}

async fn sleep_until_deadline(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(at) => sleep_until(Instant::from_std(at)).await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::sync::{mpsc, oneshot};

    use super::*;

    use crate::actor::subtree::{SubtreeHold, SubtreeLink};
    use crate::actor::supervision::{
        ChildLifecycle, ChildLinkGuard, ChildSpec, ChildState, Origin, SupervisionConfig,
        SupervisionState, TaskPlane,
    };
    use crate::system::ActorSystem;
    use crate::types::{ActorId, RestartType, StatusTx, StatusValue};

    fn dummy_system() -> Arc<ActorSystem> {
        ActorSystem::create(format!("slot-test-{}", uuid::Uuid::new_v4())).unwrap()
    }

    /// A child whose turns take `turn` each. It vetoes every request that is
    /// not a `Kill`, and ends only on a `Kill`, observed at a turn boundary.
    /// With `hang`, its first turn awaits forever, so only an abort ends it.
    /// Returns the task, its lane, its status plane and the counts of turns
    /// started and finished.
    struct Child {
        join: JoinHandle<StopReason>,
        lane: StopLane,
        status: StatusRx,
        started: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
        vetoed: Arc<AtomicUsize>,
    }

    fn spawn_vetoing_child(turn: Duration, hang: bool) -> Child {
        let (lane, mut lane_rx) = StopLane::new();
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let vetoed = Arc::new(AtomicUsize::new(0));
        let (s, f, v) = (started.clone(), finished.clone(), vetoed.clone());
        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    changed = lane_rx.changed() => {
                        if changed.is_err() {
                            return StopReason::Kill;
                        }
                        let reason = lane_rx.borrow_and_update().reason.clone();
                        if matches!(reason, Some(StopReason::Kill)) {
                            status_tx.send_replace(StatusValue::new(ActorStatus::Stopped));
                            return StopReason::Kill;
                        }
                        v.fetch_add(1, Ordering::SeqCst);
                    }
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        s.fetch_add(1, Ordering::SeqCst);
                        if hang {
                            std::future::pending::<()>().await;
                        }
                        tokio::time::sleep(turn).await;
                        f.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        });
        Child {
            join,
            lane,
            status,
            started,
            finished,
            vetoed,
        }
    }

    struct ChildStats {
        started: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
        vetoed: Arc<AtomicUsize>,
        lane: StopLane,
    }

    const ONE_TURN: Duration = Duration::from_millis(30);

    /// The longest any awaited outcome of these tests may take; a reverted
    /// build fails at this bound instead of hanging.
    const BOUND: Duration = Duration::from_secs(10);

    async fn bounded<T>(fut: impl std::future::Future<Output = T>) -> T {
        timeout(BOUND, fut)
            .await
            .expect("timed out waiting for an outcome")
    }

    // - the shared bodies ------------------------------------------------------

    /// A supervisor child's stop bounds: its window, its kill grace and its
    /// policy; or a worker's, whose own policy never counts.
    fn child_of(is_supervisor: bool, shutdown: Shutdown, kill_grace: Duration) -> OwnStop {
        OwnStop {
            is_supervisor,
            shutdown,
            kill_grace,
        }
    }

    /// A terminal plane value with a reason.
    fn ended_with(reason: StopReason) -> StatusValue {
        StatusValue {
            status: ActorStatus::Stopped,
            reason: Some(reason),
            final_at: None,
        }
    }

    /// A lost runner's incarnation: a cell that holds `own` and a plane that
    /// never reaches `Stopped`, and the sender of that plane.
    fn lost_incarnation(own: OwnStop) -> (SlotCtl, StatusTx, Arc<Verdicts>) {
        let verdicts: Arc<Verdicts> = Arc::default();
        let (ctl, io) = slot_pair(verdicts.clone());
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _lane_rx) = StopLane::new();
        io.publish(lane, status, own, None);
        drop(io);
        (ctl, status_tx, verdicts)
    }

    async fn closed_cell_and_lost_end_return_the_planes_reason(_paused: bool) {
        // A plane that ended with an inherited stop, one that ended with a
        // failure, and one that closed with no reason at all.
        let failure = || StopReason::Failure(ActorError::user("the incarnation failed"));
        for (n, reason) in [Some(StopReason::ParentKilled), Some(failure()), None]
            .into_iter()
            .enumerate()
        {
            let (ctl, status_tx, _verdicts) = lost_incarnation(child_of(
                true,
                Shutdown::Timeout(Duration::from_secs(2)),
                DEFAULT_KILL_GRACE,
            ));
            let end = ctl.lost_end().expect("an incarnation was published");
            let mut cell = ctl.watch();
            match &reason {
                Some(reason) => {
                    status_tx.send_replace(ended_with(reason.clone()));
                }
                None => drop(status_tx),
            }
            let want = reason.clone().unwrap_or(StopReason::Kill);
            match bounded(await_cell(&mut cell, n as u64)).await {
                CellOutcome::Fate(fate) => assert_eq!(fate.reason, want, "closed cell, row {n}"),
                other => panic!("row {n}: expected the incarnation's end, got {other:?}"),
            }
            assert_eq!(bounded(end.wait()).await, want, "lost end, row {n}");
            // With no fate recorded, a manual stop reads the same plane.
            assert!(ctl.fate().is_none());
            assert_eq!(ctl.end_reason(), want, "end reason, row {n}");
        }
    }

    // - each shared body on both runtime flavors -------------------------------

    /// A status plane closed without `Stopped` is an ended incarnation (its
    /// task ended and nothing is left to publish), also once a caller
    /// recorded the verdict on it: the wait reports the end, not the verdict.
    #[tokio::test]
    async fn closed_cell_reads_a_closed_plane_as_ended() {
        let (ctl, io) = slot_pair(Default::default());
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _lane_rx) = StopLane::new();
        io.publish(lane, status, OwnStop::worker(DEFAULT_KILL_GRACE), None);
        drop(io);
        drop(status_tx);
        let mut cell = ctl.watch();
        cell.lost.set();
        let outcome = timeout(Duration::from_secs(5), await_cell(&mut cell, 6))
            .await
            .expect("an ended incarnation is read at once");
        assert!(
            matches!(
                outcome,
                CellOutcome::Fate(ChildFate {
                    reason: StopReason::Kill,
                    incarnation: 6
                })
            ),
            "{outcome:?}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn closed_cell_and_lost_end_return_the_planes_reason_current_thread() {
        closed_cell_and_lost_end_return_the_planes_reason(true).await;
    }

    // - a failed task of either plane -------------------------------------------

    fn state_with_a_child(id: &str, ctl: SlotCtl, incarnation: u64) -> SupervisionState {
        let mut sup = SupervisionState::new(SupervisionConfig::default());
        sup.registry.register(ChildState {
            id: ActorId::from(id),
            name: Some(Arc::from(id.to_string())),
            spec: ChildSpec {
                restart_type: RestartType::Permanent,
                shutdown: Shutdown::default(),
                start_timeout: None,
                kill_grace: crate::actor::supervision::DEFAULT_KILL_GRACE,
                is_supervisor: false,
            },
            ctl,
            lifecycle: ChildLifecycle::Running(incarnation),
            origin: Origin::Initial,
            predecessor: None,
        });
        sup
    }

    async fn runner_join_error_becomes_a_recorded_death_after_the_incarnation_ends() {
        let (ctl, io) = slot_pair(Default::default());
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _lane_rx) = StopLane::new();
        io.publish(lane, status, OwnStop::worker(DEFAULT_KILL_GRACE), None);
        let mut sup = state_with_a_child("lost", ctl, 5);
        // The runner stand-in holds the slot and the lane, and fails.
        let task = sup.death_set.spawn(async move {
            let _io = io;
            tokio::task::yield_now().await;
            if std::hint::black_box(true) {
                panic!("runner failed");
            }
            (ActorId::from("lost"), StopReason::Graceful, 5)
        });
        sup.task_owner
            .insert(task.id(), (ActorId::from("lost"), 5, TaskPlane::Death));
        let err = sup
            .death_set
            .join_next_with_id()
            .await
            .unwrap()
            .expect_err("the runner failed");
        let (panic, _) = sup.lost_death_task(err);
        assert!(matches!(&panic, Some(ActorError::Panic(text)) if text == "runner failed"));
        assert_eq!(
            sup.task_owner.len(),
            1,
            "the failed task's entry was replaced by the replacement task's"
        );
        assert!(
            sup.task_owner
                .values()
                .all(|(_, _, plane)| *plane == TaskPlane::Death),
            "the replacement follows the incarnation on the death plane"
        );

        // No report while the incarnation lives (its status sender is open).
        assert!(
            timeout(Duration::from_millis(100), sup.death_set.join_next())
                .await
                .is_err(),
            "the death is not reported while the incarnation lives"
        );
        // One report after it ends; a status sender dropped without
        // `Stopped` reads as an abort.
        drop(status_tx);
        let (id, reason, incarnation) = bounded(sup.death_set.join_next()).await.unwrap().unwrap();
        assert_eq!(id.as_str(), "lost");
        assert_eq!(incarnation, 5);
        assert!(matches!(reason, StopReason::Kill), "{reason:?}");
        assert!(
            bounded(sup.death_set.join_next()).await.is_none(),
            "reported once"
        );
    }

    #[test]
    fn a_reader_that_sees_the_published_verdict_finds_the_flag_set() {
        // The flag (which logs) is set before the cell publishes the verdict,
        // so no waiter sees `Unresponsive` before the warning exists.
        for _ in 0..20_000 {
            let (ctl, io) = slot_pair_for(Arc::default(), Some((ActorId::from("order"), 1)));
            let reader = std::thread::spawn(move || {
                while !ctl.cell_rx.borrow().unresponsive {
                    std::hint::spin_loop();
                }
                ctl.lost.is_set()
            });
            io.write_verdict();
            assert!(reader.join().unwrap(), "the verdict was published first");
        }
    }

    #[tokio::test]
    async fn initiate_registers_the_attempt_on_the_restart_plane() {
        let (ctl, _io) = slot_pair(Default::default());
        let mut sup = state_with_a_child("restarted", ctl, 5);
        if let Some(child) = sup.registry.get_mut(&ActorId::from("restarted")) {
            child.lifecycle = ChildLifecycle::Down {
                incarnation: 5,
                pending: crate::actor::supervision::DownPending::None,
            };
        }
        sup.restart_fns.insert(
            ActorId::from("restarted"),
            Box::new(|incarnation, io, _predecessor| {
                Box::pin(async move {
                    let _io = io;
                    std::future::pending::<()>().await;
                    RestartOutcome::Cancelled {
                        child_id: ActorId::from("restarted"),
                        incarnation,
                        drop_panic: None,
                    }
                })
            }),
        );
        assert!(sup.initiate(&ActorId::from("restarted"), 9, Origin::Strategy));
        let owners: Vec<_> = sup.task_owner.values().cloned().collect();
        assert_eq!(
            owners,
            vec![(ActorId::from("restarted"), 9, TaskPlane::Restart)]
        );
        assert_eq!(sup.restart_set.len(), 1);
        assert!(sup.death_set.is_empty());
    }

    async fn restart_task_join_error_becomes_a_recorded_death_after_the_incarnation_ends() {
        // An attempt that published its incarnation's status and then failed:
        // the attempt's end is followed to the incarnation's end.
        let (ctl, io) = slot_pair(Default::default());
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _lane_rx) = StopLane::new();
        io.publish(lane, status, OwnStop::worker(DEFAULT_KILL_GRACE), None);
        let mut sup = state_with_a_child("attempt-lost", ctl, 7);
        if let Some(child) = sup.registry.get_mut(&ActorId::from("attempt-lost")) {
            child.lifecycle = ChildLifecycle::Restarting {
                incarnation: 7,
                next: 8,
            };
        }
        let task = sup.restart_set.spawn(async move {
            let _io = io;
            tokio::task::yield_now().await;
            if std::hint::black_box(true) {
                panic!("attempt failed after spawning");
            }
            crate::actor::supervision::RestartOutcome::Failed {
                child_id: ActorId::from("attempt-lost"),
                incarnation: 8,
                reason: StopReason::Graceful,
            }
        });
        sup.task_owner.insert(
            task.id(),
            (ActorId::from("attempt-lost"), 8, TaskPlane::Restart),
        );
        let Err(err) = bounded(sup.restart_set.join_next_with_id()).await.unwrap() else {
            panic!("the attempt task failed");
        };
        let (outcome, panic, _) = sup.lost_restart_task(err);
        assert!(
            outcome.is_none(),
            "nothing is reported before the incarnation ends"
        );
        assert!(
            matches!(&panic, Some(ActorError::Panic(text)) if text == "attempt failed after spawning")
        );
        assert_eq!(sup.task_owner.len(), 1);
        assert!(
            sup.task_owner
                .values()
                .all(|(_, _, plane)| *plane == TaskPlane::Death),
            "the replacement of an attempt follows the incarnation on the death plane"
        );

        // No report while the incarnation lives.
        assert!(
            timeout(Duration::from_millis(100), sup.death_set.join_next())
                .await
                .is_err(),
            "the death is not reported while the incarnation lives"
        );
        // Exactly one report after it ends: a status sender dropped without
        // `Stopped` reads as an abort.
        drop(status_tx);
        let (id, reason, incarnation) = bounded(sup.death_set.join_next()).await.unwrap().unwrap();
        assert_eq!((id.as_str(), incarnation), ("attempt-lost", 8));
        assert!(matches!(reason, StopReason::Kill), "{reason:?}");
        assert!(
            bounded(sup.death_set.join_next()).await.is_none(),
            "reported once"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn restart_task_join_error_becomes_a_recorded_death_after_the_incarnation_ends_current_thread(
    ) {
        restart_task_join_error_becomes_a_recorded_death_after_the_incarnation_ends().await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn runner_join_error_becomes_a_recorded_death_after_the_incarnation_ends_current_thread()
    {
        runner_join_error_becomes_a_recorded_death_after_the_incarnation_ends().await;
    }

    // - restart attempts ----------------------------------------------------------

    struct AttemptRig {
        ctl: SlotCtl,
        run: tokio::task::JoinHandle<RestartOutcome>,
        child: ChildStats,
        verdicts: Arc<Verdicts>,
        _ack: oneshot::Sender<Result<(), ActorError>>,
        _reaper: mpsc::UnboundedReceiver<crate::system::ReaperEntry>,
    }

    /// An attempt over `child`, whose init never acknowledges.
    fn attempt_rig(child: Child, start_timeout: Option<Duration>, grace: Duration) -> AttemptRig {
        let verdicts: Arc<Verdicts> = Arc::default();
        let (ctl, io) = slot_pair(verdicts.clone());
        io.publish(
            child.lane.clone(),
            child.status.clone(),
            OwnStop::worker(DEFAULT_KILL_GRACE),
            None,
        );
        let abort = child.join.abort_handle();
        let (reaper_tx, reaper) = mpsc::unbounded_channel();
        let guard = ChildLinkGuard::new(
            child.lane.clone(),
            abort,
            reaper_tx,
            crate::actor::supervision::rules::InheritedStop::Kill {
                grace: DEFAULT_KILL_GRACE,
            },
            dummy_system(),
            tokio::runtime::Handle::current(),
        );
        let (ack_tx, ack_rx) = oneshot::channel();
        let stats = ChildStats {
            started: child.started.clone(),
            finished: child.finished.clone(),
            vetoed: child.vetoed.clone(),
            lane: child.lane.clone(),
        };
        let run = tokio::spawn(run_attempt(Attempt {
            child_id: ActorId::from("attempt-child"),
            incarnation: 3,
            join: child.join,
            lane: child.lane,
            guard,
            ack: ack_rx,
            start_timeout,
            grace,
            io,
        }));
        AttemptRig {
            ctl,
            run,
            child: stats,
            verdicts,
            _ack: ack_tx,
            _reaper: reaper,
        }
    }

    async fn cancel_flag_cancels_an_attempt_before_its_factory_and_is_ignored_after(_paused: bool) {
        // Before the factory: the flag alone, with no order, cancels.
        let (ctl, mut io) = slot_pair(Default::default());
        assert!(!cancelled_before_factory(&mut io), "nothing written");
        assert!(!ctl.has_order() && !ctl.cancel_written());
        ctl.cancel_unspawned();
        assert!(ctl.cancel_written());
        assert!(!ctl.has_order(), "the flag is not an order");
        assert!(
            cancelled_before_factory(&mut io),
            "the flag cancels the attempt"
        );
        drop(io);
        let mut watch = ctl.watch();
        match bounded(await_cell(&mut watch, 1)).await {
            CellOutcome::Cancelled => {}
            other => panic!("expected the cancel, got {other:?}"),
        }

        // While the attempt waits for its predecessor's subtree, which still
        // runs: the flag alone cancels it, whether it was written before the
        // wait began or while it ran. A live supervisor child with an
        // unbounded orderly stop below the predecessor leaves the wait without
        // a deadline, so only the flag can end it, however slow the host.
        let hold = SubtreeHold::new(None);
        let below = SubtreeHold::new(Some(SubtreeLink {
            parent: hold.token(),
            own: Duration::ZERO,
            infinity: true,
        }));
        let predecessor = Predecessor::untracked(hold.watch());
        assert_eq!(predecessor.subtree.release_bound(), None, "unbounded");
        let (ctl, mut io) = slot_pair(Default::default());
        assert_eq!(
            bounded(async {
                tokio::select! {
                    action = await_predecessor(Some(&predecessor), &mut io) => Some(action),
                    () = tokio::time::sleep(Duration::from_millis(50)) => None,
                }
            })
            .await,
            None,
            "nothing written: the wait goes on"
        );
        ctl.cancel_unspawned();
        assert_eq!(
            bounded(await_predecessor(Some(&predecessor), &mut io)).await,
            WaitAction::Cancel,
            "written before the wait"
        );
        let (ctl, mut io) = slot_pair(Default::default());
        let waiting = tokio::spawn(async move {
            let action = await_predecessor(Some(&predecessor), &mut io).await;
            (action, io)
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "the wait goes on");
        ctl.cancel_unspawned();
        let (action, io) = bounded(waiting).await.unwrap();
        assert_eq!(action, WaitAction::Cancel, "written during the wait");
        assert!(!ctl.has_order(), "the flag is not an order");
        drop(io);
        drop(below);
        drop(hold);

        // After the spawn: the flag changes nothing. The attempt keeps
        // waiting for its init, and only an order ends it.
        let rig = attempt_rig(
            spawn_vetoing_child(ONE_TURN, false),
            None,
            DEFAULT_KILL_GRACE,
        );
        // The attempt is running its wait for the init when the flag arrives.
        tokio::time::sleep(Duration::from_millis(50)).await;
        rig.ctl.cancel_unspawned();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!rig.run.is_finished(), "a spawned attempt ignores the flag");
        assert!(
            rig.child.lane.reason().is_none(),
            "the flag raised nothing on the incarnation's lane"
        );
        assert!(!rig.ctl.has_order());
        assert!(!rig.ctl.is_unresponsive());
        rig.ctl
            .order_stop(Shutdown::Timeout(Duration::from_millis(50)), false);
        match bounded(rig.run).await.unwrap() {
            RestartOutcome::Stopped { .. } => {}
            _ => panic!("expected the incarnation to end under the order"),
        }
    }

    async fn await_cell_returns_cancelled_for_an_attempt_that_cancelled_and_dropped_its_sender(
        _paused: bool,
    ) {
        let (ctl, io) = slot_pair(Default::default());
        io.mark_cancelled();
        drop(io);
        let mut watch = ctl.watch();
        match bounded(await_cell(&mut watch, 2)).await {
            CellOutcome::Cancelled => {}
            other => panic!("a cancel is not read as no incarnation: {other:?}"),
        }
        // An executor that ended without any incarnation or cancel is none.
        let (ctl, io) = slot_pair(Default::default());
        drop(io);
        let mut watch = ctl.watch();
        match bounded(await_cell(&mut watch, 2)).await {
            CellOutcome::NoIncarnation => {}
            other => panic!("expected no incarnation, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancel_flag_cancels_an_attempt_before_its_factory_and_is_ignored_after_current_thread()
    {
        cancel_flag_cancels_an_attempt_before_its_factory_and_is_ignored_after(true).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn await_cell_returns_cancelled_for_an_attempt_that_cancelled_and_dropped_its_sender_current_thread(
    ) {
        await_cell_returns_cancelled_for_an_attempt_that_cancelled_and_dropped_its_sender(true)
            .await;
    }

    /// The waits that can give the Unresponsive verdict, against a stop whose
    /// `Kill` stage the reaper raises late. The reaper is held back by a gate:
    /// the guard's two-stage entry (`Kill` after the child's window, the abort
    /// one kill grace after that `Kill`) is fed only `late` after its `Kill`
    /// instant, which is how an entry behaves whose `Kill` stage fires late,
    /// since the entry counts the grace again from the moment its `Kill`
    /// fires. An obeying incarnation ends inside the grace of its `Kill`, so a
    /// verdict for it is wrong; a stuck one (its status plane held open past
    /// its task) gets the verdict exactly the settle time after its abort,
    /// never before; an abort owed and never issued gives it at the cap.
    mod late_kill {
        use std::sync::Mutex;

        use super::*;
        use crate::actor::subtree::SubtreeLink;
        use crate::actor::supervision::rules::own_budget;
        use crate::actor::supervision::ABORT_SETTLE;
        use crate::system::ReaperEntry;

        const WINDOW: Duration = Duration::from_millis(400);
        const GRACE: Duration = Duration::from_millis(100);
        /// How long after its `Kill` an obeying incarnation ends: inside its
        /// grace.
        const OBEY: Duration = Duration::from_millis(90);
        /// How late the `Kill` stage fires.
        const LATE: Duration = Duration::from_millis(250);

        fn own() -> OwnStop {
            OwnStop {
                is_supervisor: true,
                shutdown: Shutdown::Timeout(WINDOW),
                kill_grace: GRACE,
            }
        }

        #[derive(Clone, Copy, PartialEq)]
        enum Kind {
            /// Ends `OBEY` after a `Kill`.
            Obeys,
            /// Ignores every request; its abort ends its task, and its
            /// status plane stays open, as for a task that never yields.
            Stuck,
        }

        struct Occupant {
            join: JoinHandle<()>,
            lane: StopLane,
            status: StatusRx,
            /// Holds a stuck occupant's plane open past its task.
            _plane: Option<StatusTx>,
            /// Holds a stuck occupant's subtree alive past its task.
            _kept: Option<Arc<SubtreeHold>>,
            ended: Arc<Mutex<Option<Instant>>>,
        }

        /// An incarnation of `kind`. With `link`, it is a child in a subtree,
        /// its lane's record linked to its token as a spawn links it.
        fn occupant(kind: Kind, link: Option<SubtreeLink>) -> Occupant {
            let (lane, mut lane_rx) = StopLane::new();
            let hold =
                link.map(|link| SubtreeHold::with_record(Some(link), lane.abort_record().clone()));
            let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
            let plane = (kind == Kind::Stuck).then(|| status_tx.clone());
            let kept = hold.clone().filter(|_| kind == Kind::Stuck);
            let ended = Arc::new(Mutex::new(None));
            let e = ended.clone();
            let join = tokio::spawn(async move {
                let _hold = hold;
                loop {
                    if lane_rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                    let kill = matches!(lane_rx.borrow_and_update().reason, Some(StopReason::Kill));
                    if kill && kind == Kind::Obeys {
                        break;
                    }
                }
                tokio::time::sleep(OBEY).await;
                *e.lock().unwrap() = Some(Instant::now());
                status_tx.send_replace(StatusValue::new(ActorStatus::Stopped));
            });
            Occupant {
                join,
                lane,
                status,
                _plane: plane,
                _kept: kept,
                ended,
            }
        }

        /// The firing guard of a supervisor child: `ParentKilled` at once,
        /// and the two-stage entry, owed from now, fed to the reaper `late`
        /// after its `Kill` instant.
        fn guard_fires(occ: &Occupant, late: Duration) -> (Instant, Arc<ActorSystem>) {
            let fired = Instant::now();
            occ.lane.raise(StopReason::ParentKilled);
            let kill_at = fired + WINDOW;
            let entry = ReaperEntry::two_stage(
                kill_at,
                occ.lane.clone(),
                kill_at + GRACE,
                occ.join.abort_handle(),
            );
            let system = dummy_system();
            let feeder = system.clone();
            tokio::spawn(async move {
                tokio::time::sleep_until(kill_at + late).await;
                feeder.feed_reaper(&tokio::runtime::Handle::current(), entry);
            });
            (fired, system)
        }

        /// The instant of the slot's verdict, once it is recorded.
        fn verdict_watch(verdicts: &Arc<Verdicts>) -> JoinHandle<Instant> {
            let verdicts = verdicts.clone();
            tokio::spawn(async move {
                verdicts.notified().await;
                Instant::now()
            })
        }

        /// The abort instant of an entry whose `Kill` fires `late`.
        fn abort_at(fired: Instant, late: Duration) -> Instant {
            fired + WINDOW + late + GRACE
        }

        // - LostEnd::wait -------------------------------------------------------

        /// An abort that becomes owed after the wait began still holds the
        /// verdict: the wait reads the record again when it reaches its
        /// deadline, and an entry that never fires gives the verdict at the
        /// cap.
        #[tokio::test(start_paused = true)]
        async fn a_lost_end_reads_an_abort_owed_after_it_began() {
            let verdicts: Arc<Verdicts> = Arc::default();
            let (ctl, io) = slot_pair(verdicts.clone());
            let occ = occupant(Kind::Stuck, None);
            io.publish(occ.lane.clone(), occ.status.clone(), own(), None);
            let watcher = verdict_watch(&verdicts);
            let started = Instant::now();
            let waiter = tokio::spawn(ctl.lost_end().unwrap().wait());
            tokio::time::sleep(Duration::from_millis(10)).await;
            assert_eq!(occ.lane.abort_record().view(), (None, false));
            let entry = ReaperEntry::two_stage(
                Instant::now() + WINDOW,
                occ.lane.clone(),
                Instant::now() + WINDOW + GRACE,
                occ.join.abort_handle(),
            );
            let at = bounded(watcher).await.unwrap();
            let bound = lost_end_bound(own());
            assert_eq!(at, started + bound + bound);
            drop(entry);
            waiter.abort();
        }

        // - closed_cell -----------------------------------------------------------

        // - SubtreeWait: await_subtree and await_predecessor ------------------------

        /// The subtree of a previous incarnation: one supervisor child of
        /// `kind` below the root, the root already ended.
        fn ended_root_with_a_child(kind: Kind) -> (Predecessor, Occupant) {
            let root = SubtreeHold::new(None);
            let link = SubtreeLink {
                parent: root.token(),
                own: own_budget(true, Shutdown::Timeout(WINDOW), GRACE).unwrap(),
                infinity: false,
            };
            let occ = occupant(kind, Some(link));
            let predecessor = Predecessor::untracked(root.watch());
            drop(root);
            (predecessor, occ)
        }

        #[tokio::test(start_paused = true)]
        async fn a_restart_attempt_escalates_a_stuck_child_the_settle_time_after_a_late_abort() {
            let (predecessor, occ) = ended_root_with_a_child(Kind::Stuck);
            let (_ctl, mut io) = slot_pair(Default::default());
            let (fired, _system) = guard_fires(&occ, LATE);
            let action = bounded(await_predecessor(Some(&predecessor), &mut io)).await;
            assert_eq!(action, WaitAction::Escalate);
            assert_eq!(Instant::now(), abort_at(fired, LATE) + ABORT_SETTLE);
            occ.join.abort();
        }

        // - Writers ---------------------------------------------------------------

        /// An entry dropped without firing aborts its task and records that
        /// abort: a wait held for it gives the verdict the settle time after
        /// the drop, not at its cap.
        #[tokio::test(start_paused = true)]
        async fn a_dropped_entry_releases_a_held_wait_the_settle_time_after_its_drop() {
            let (ctl, io) = slot_pair(Default::default());
            let occ = occupant(Kind::Stuck, None);
            io.publish(occ.lane.clone(), occ.status.clone(), own(), None);
            drop(io);
            let started = Instant::now();
            let entry = ReaperEntry::two_stage(
                started + WINDOW,
                occ.lane.clone(),
                started + WINDOW + GRACE,
                occ.join.abort_handle(),
            );
            let dropped_at = started + lost_end_bound(own()) + GRACE;
            tokio::spawn(async move {
                tokio::time::sleep_until(dropped_at).await;
                drop(entry);
            });
            let outcome = bounded(await_cell(&mut ctl.watch(), 1)).await;
            assert!(matches!(outcome, CellOutcome::Unresponsive), "{outcome:?}");
            assert_eq!(Instant::now(), dropped_at + ABORT_SETTLE);
        }

        /// The ladder's own abort is recorded on the incarnation's lane at
        /// the end of its grace, and its `Kill` owes that abort until then.
        #[tokio::test(start_paused = true)]
        async fn a_ladders_abort_is_recorded_on_the_incarnations_lane() {
            let child = tokio::spawn(std::future::pending::<StopReason>());
            let (lane, _lane_rx) = StopLane::new();
            let (ctl, mut io) = slot_pair(Default::default());
            ctl.write_order(Shutdown::Kill, false);
            let mut join = child;
            let started = Instant::now();
            let ladder = tokio::spawn({
                let lane = lane.clone();
                async move { run_ladder(&mut join, &lane, &mut io, GRACE).await }
            });
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            assert_eq!(
                lane.abort_record().view(),
                (None, true),
                "owed after the Kill"
            );
            let reason = bounded(ladder).await.unwrap();
            assert!(matches!(reason, StopReason::Kill), "{reason:?}");
            assert_eq!(lane.abort_record().view(), (Some(started + GRACE), false));
            drop(ctl);
        }

        /// A restart attempt whose initialization outlives its start timeout:
        /// the attempt's ladder raises `Kill` then, owes its abort through
        /// the grace, and records that abort on the incarnation's lane once
        /// the grace has passed.
        #[tokio::test(start_paused = true)]
        async fn an_attempts_start_timeout_abort_is_recorded_on_the_incarnations_lane() {
            const START: Duration = Duration::from_millis(200);
            let (lane, lane_rx) = StopLane::new();
            let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
            let join = tokio::spawn(async move {
                let _lane_rx = lane_rx;
                let _status = status_tx;
                std::future::pending::<StopReason>().await
            });
            let child = Child {
                join,
                lane: lane.clone(),
                status,
                started: Arc::default(),
                finished: Arc::default(),
                vetoed: Arc::default(),
            };
            let started = Instant::now();
            let rig = attempt_rig(child, Some(START), GRACE);
            tokio::time::sleep(START + Duration::from_millis(1)).await;
            assert!(
                matches!(lane.reason(), Some(StopReason::Kill)),
                "the start timeout raised Kill"
            );
            assert_eq!(
                lane.abort_record().view(),
                (None, true),
                "owed through the grace"
            );
            let outcome = bounded(rig.run).await.unwrap();
            assert!(
                matches!(outcome, RestartOutcome::Failed { .. }),
                "the attempt reports its failed start"
            );
            assert_eq!(
                lane.abort_record().view(),
                (Some(started + START + GRACE), false)
            );
        }

        // - The control -----------------------------------------------------------

        // - A task that ends while a hold outlives it ---------------------------

        /// A hold on the incarnation's token kept past its task, as a stream
        /// forwarder still ending keeps it, linked below `root` with the
        /// lane's record.
        fn kept_below(root: &SubtreeHold, lane: &StopLane) -> Arc<SubtreeHold> {
            SubtreeHold::with_record(
                Some(SubtreeLink {
                    parent: root.token(),
                    own: Duration::ZERO,
                    infinity: false,
                }),
                lane.abort_record().clone(),
            )
        }

        /// A task that obeys the first `Kill` it reads, `OBEY` later, and
        /// publishes `Stopped` when it ends if `publish_stopped` says so.
        fn obeying(
            mut lane_rx: crate::types::LaneRx,
            status_tx: StatusTx,
            publish_stopped: bool,
        ) -> JoinHandle<StopReason> {
            tokio::spawn(async move {
                loop {
                    if lane_rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                    if matches!(lane_rx.borrow_and_update().reason, Some(StopReason::Kill)) {
                        break;
                    }
                }
                tokio::time::sleep(OBEY).await;
                if publish_stopped {
                    status_tx.send_replace(StatusValue::new(ActorStatus::Stopped));
                }
                drop(status_tx);
                StopReason::Kill
            })
        }

        /// The runner's ladder raises a `Kill` it owes an abort for, the
        /// incarnation obeys inside the grace, and the hold is kept past the
        /// task: the join releases the mark, and the end counts as the
        /// subtree's latest abort.
        #[tokio::test(start_paused = true)]
        async fn a_runner_that_sees_its_task_end_releases_the_mark_its_kill_took() {
            let root = SubtreeHold::new(None);
            let (lane, lane_rx) = StopLane::new();
            let kept = kept_below(&root, &lane);
            let (status_tx, _status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
            let mut join = obeying(lane_rx, status_tx, false);
            let (ctl, mut io) = slot_pair(Default::default());
            let started = Instant::now();
            ctl.write_order(Shutdown::Kill, false);
            let ladder = tokio::spawn({
                let lane = lane.clone();
                async move { run_ladder(&mut join, &lane, &mut io, GRACE).await }
            });
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            assert_eq!(
                lane.abort_record().view(),
                (None, true),
                "owed after the Kill"
            );
            assert_eq!(root.watch().abort_view(), (None, true));
            let reason = bounded(ladder).await.unwrap();
            assert!(matches!(reason, StopReason::Kill), "{reason:?}");
            assert_eq!(lane.abort_record().view(), (None, false));
            assert_eq!(
                root.watch().abort_view(),
                (Some(started + OBEY), false),
                "the end is the subtree's latest abort and nothing stays owed"
            );
            drop((ctl, kept, root));
        }

        /// The attempt's start timeout raises a `Kill` it owes an abort for,
        /// the incarnation obeys inside the grace, and the hold is kept past
        /// the task.
        #[tokio::test(start_paused = true)]
        async fn an_attempt_that_sees_its_task_end_releases_the_mark_its_kill_took() {
            const START: Duration = Duration::from_millis(200);
            let root = SubtreeHold::new(None);
            let (lane, lane_rx) = StopLane::new();
            let kept = kept_below(&root, &lane);
            let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
            let join = obeying(lane_rx, status_tx, false);
            let child = Child {
                join,
                lane: lane.clone(),
                status,
                started: Arc::default(),
                finished: Arc::default(),
                vetoed: Arc::default(),
            };
            let started = Instant::now();
            let rig = attempt_rig(child, Some(START), GRACE);
            tokio::time::sleep(START + Duration::from_millis(1)).await;
            assert_eq!(
                lane.abort_record().view(),
                (None, true),
                "owed after the Kill"
            );
            let outcome = bounded(rig.run).await.unwrap();
            assert!(
                matches!(outcome, RestartOutcome::Failed { .. }),
                "a start timeout fails the attempt"
            );
            assert_eq!(lane.abort_record().view(), (None, false));
            assert_eq!(
                root.watch().abort_view(),
                (Some(started + START + OBEY), false)
            );
            drop((kept, root));
        }

        /// A `Kill` another party stored and an initialization that ends only
        /// after its task ended, with the hold kept past the task: the
        /// attempt's kill path runs over an ended task and owes it nothing.
        #[tokio::test(start_paused = true)]
        async fn an_attempts_kill_path_owes_nothing_to_a_task_that_already_ended() {
            let root = SubtreeHold::new(None);
            let (lane, lane_rx) = StopLane::new();
            let kept = kept_below(&root, &lane);
            let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
            lane.raise(StopReason::Kill);
            let join = tokio::spawn(async move {
                let _lane_rx = lane_rx;
                let _status = status_tx;
                StopReason::Kill
            });
            while !join.is_finished() {
                tokio::task::yield_now().await;
            }
            let child = Child {
                join,
                lane: lane.clone(),
                status,
                started: Arc::default(),
                finished: Arc::default(),
                vetoed: Arc::default(),
            };
            let AttemptRig {
                run,
                _ack,
                ctl,
                _reaper,
                ..
            } = attempt_rig(child, None, GRACE);
            drop(_ack);
            let outcome = bounded(run).await.unwrap();
            assert!(
                matches!(outcome, RestartOutcome::Failed { .. }),
                "a failed start"
            );
            assert_eq!(lane.abort_record().view(), (None, false));
            assert_eq!(
                root.watch().abort_view(),
                (None, false),
                "nothing was owed, so the end raised nothing"
            );
            drop((ctl, kept, root));
        }

        /// The lane took a `Kill` and the task ended before the ladder owed
        /// its abort, with the lane's receiver still held: the ladder owes
        /// the ended task nothing.
        #[tokio::test(start_paused = true)]
        async fn a_ladders_raise_over_an_ended_task_owes_nothing() {
            let root = SubtreeHold::new(None);
            let (lane, _lane_rx) = StopLane::new();
            let kept = kept_below(&root, &lane);
            let join = tokio::spawn(async { StopReason::Kill });
            while !join.is_finished() {
                tokio::task::yield_now().await;
            }
            raise_owing(&lane, StopReason::Kill, &join.abort_handle());
            assert_eq!(lane.abort_record().view(), (None, false));
            assert_eq!(root.watch().abort_view(), (None, false));
            drop((kept, root));
        }

        /// A lost runner: the guard fired (a two-stage entry owes the abort
        /// from the moment it exists), the incarnation obeys the `Kill` of
        /// the entry's first stage and ends well inside the entry's grace,
        /// its hold kept past the task. Returns what the subtree reads when
        /// the observer of the plane's end returns, the instant of that
        /// return, and the answer and the delay of a successor wait started
        /// then, with the hold dropped `drop_after` later.
        async fn lost(
            closed: bool,
            drop_after: Duration,
        ) -> ((Option<Instant>, bool), Instant, WaitAction, Duration) {
            const ENTRY_GRACE: Duration = Duration::from_millis(300);
            let root = SubtreeHold::new(None);
            let (lane, lane_rx) = StopLane::new();
            let kept = kept_below(&root, &lane);
            let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
            let join = obeying(lane_rx, status_tx, true);
            let own = OwnStop {
                is_supervisor: true,
                shutdown: Shutdown::Timeout(WINDOW),
                kill_grace: ENTRY_GRACE,
            };
            let (ctl, io) = slot_pair(Default::default());
            io.publish(lane.clone(), status, own, None);
            if closed {
                drop(io);
            }
            let fired = Instant::now();
            lane.raise(StopReason::ParentKilled);
            let kill_at = fired + WINDOW;
            let entry = ReaperEntry::two_stage(
                kill_at,
                lane.clone(),
                kill_at + ENTRY_GRACE,
                join.abort_handle(),
            );
            let system = dummy_system();
            system.feed_reaper(&tokio::runtime::Handle::current(), entry);
            if closed {
                let mut watch = ctl.watch();
                let outcome = bounded(await_cell(&mut watch, 1)).await;
                assert!(matches!(outcome, CellOutcome::Fate(_)), "{outcome:?}");
            } else {
                let reason = bounded(ctl.lost_end().unwrap().wait()).await;
                assert!(matches!(reason, StopReason::Kill), "{reason:?}");
            }
            let seen_at = Instant::now();
            let at_return = root.watch().abort_view();
            let predecessor = Predecessor::untracked(root.watch());
            drop(root);
            let dropper = tokio::spawn(async move {
                tokio::time::sleep(drop_after).await;
                drop(kept);
            });
            let action = bounded(await_subtree(&predecessor)).await;
            let waited = Instant::now() - seen_at;
            dropper.abort();
            drop(system);
            (at_return, seen_at, action, waited)
        }

        /// The observer of a lost runner's incarnation returns on the end of
        /// its plane and releases the mark the guard's entry owes: a hold
        /// kept past the task keeps nothing owed, and a wait on the subtree
        /// gives its verdict the settle time after the end.
        #[tokio::test(start_paused = true)]
        async fn a_lost_end_releases_the_mark_when_its_plane_ends() {
            let (at, seen_at, action, waited) = lost(false, Duration::from_millis(1_000)).await;
            assert_eq!(at, (Some(seen_at), false), "the end is the latest abort");
            assert_eq!(action, WaitAction::Escalate);
            assert_eq!(waited, ABORT_SETTLE);
        }

        /// As for the lost end, for the wait on a cell whose executor is
        /// gone.
        #[tokio::test(start_paused = true)]
        async fn a_closed_cell_releases_the_mark_when_its_plane_ends() {
            let (at, seen_at, action, waited) = lost(true, Duration::from_millis(1_000)).await;
            assert_eq!(at, (Some(seen_at), false), "the end is the latest abort");
            assert_eq!(action, WaitAction::Escalate);
            assert_eq!(waited, ABORT_SETTLE);
        }
    }
}
