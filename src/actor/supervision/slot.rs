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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
use crate::system::wait_end_or_floor;
use crate::system::{raise_noted, read_status};
use crate::telemetry::{ActorRef, AttemptOutcome, EventKind, TelemetryEvent};
use crate::types::{
    ActorId, ActorStatus, ChildFate, RaiseResult, Shutdown, StatusRx, StopLane, StopReason,
    StopSeverity,
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
    /// The incarnation's cell already read ended when the order was written,
    /// so the order did not end it. Written by the supervisor only.
    pub after_end: bool,
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

/// What the executor tells the supervisor. `lane`, `status` and `subtree`
/// are written when the incarnation is spawned: by the spawn itself, before
/// the incarnation's task can run, for a restart attempt, and right after the
/// spawn for a first start.
#[derive(Clone, Debug, Default)]
pub(crate) struct SlotCell {
    /// The incarnation's end, written once, after its task fully exited.
    pub fate: Option<ChildFate>,
    /// Set when the incarnation outlived its whole ladder, abort included.
    pub unresponsive: bool,
    /// Set by a restart attempt that honored a stop order before it called
    /// its factory: no incarnation existed.
    pub cancelled: bool,
    /// Set by a restart attempt whose start timeout raised `Kill` on its
    /// incarnation, before the raise: the attempt reports the start timeout,
    /// and so does a supervisor that reports the attempt from the
    /// incarnation's status plane.
    pub timeout_cut: bool,
    /// Set by a restart attempt whose start timeout fired before it read
    /// the acknowledgment, whether or not it raised `Kill`, before the raise:
    /// from then on the attempt hands nothing over.
    pub timeout_fired: bool,
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
    /// The telemetry ticket of each restart attempt in flight, by child and
    /// incarnation; empty while telemetry is off. An entry leaves when the
    /// attempt's outcome is taken, and a ticket still here when the last
    /// holder of the signal lets go lowers the supervisor's count as it
    /// drops. Kept here, behind the supervisor's `Arc`, so the supervision
    /// state itself carries nothing for telemetry.
    tickets: std::sync::Mutex<HashMap<(ActorId, u64), AttemptTicket>>,
}

impl Verdicts {
    /// Keeps the ticket of the attempt of `child` as `incarnation`.
    pub(crate) fn keep_ticket(&self, child: &ActorId, incarnation: u64, mut ticket: AttemptTicket) {
        ticket.stored = Some((child.clone(), incarnation));
        self.tickets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert((child.clone(), incarnation), ticket);
    }

    /// Takes the ticket of the attempt of `child` as `incarnation`, if it has
    /// one. With no ticket kept, one lock and no allocation.
    pub(crate) fn take_ticket(&self, child: &ActorId, incarnation: u64) -> Option<AttemptTicket> {
        let mut tickets = self
            .tickets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if tickets.is_empty() {
            return None;
        }
        tickets.remove(&(child.clone(), incarnation))
    }

    /// Settles every ticket still kept, each as an attempt that ended with
    /// the supervisor and reached no outcome: called once the supervisor has
    /// collected everything its planes held, ahead of its own end event.
    pub(crate) fn settle_unsettled(&self) {
        let kept: Vec<_> = {
            let mut tickets = self
                .tickets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tickets.drain().collect()
        };
        for ((child, incarnation), ticket) in kept {
            ticket.settle(&child, incarnation, AttemptOutcome::Abandoned);
        }
    }

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
    cell: Arc<VerdictCell>,
    verdicts: Arc<Verdicts>,
    key: Option<(ActorId, u64)>,
}

/// What one incarnation's verdict flag shares among its holders: whether the
/// verdict was recorded, and, for the supervisor's check of a verdict nobody
/// will report, how many waits on the incarnation's fate cell are open and
/// whether a wait was handed the verdict. One allocation, made with the slot.
#[derive(Default)]
struct VerdictCell {
    set: AtomicBool,
    open: AtomicUsize,
    reported: AtomicBool,
}

impl VerdictFlag {
    /// Records the verdict unless it was recorded already. Returns whether
    /// this call recorded it.
    #[cfg(test)]
    pub(crate) fn set(&self) -> bool {
        self.record(LADDER_VERDICT, None)
    }

    /// As [`set`](Self::set), naming the incarnation's abort record, through
    /// which the verdict's event finds its system.
    pub(crate) fn set_for(&self, record: Option<&AbortRecord>) -> bool {
        self.record(LADDER_VERDICT, record)
    }

    /// Records the verdict of a wait for the incarnation's subtree that
    /// outlived its bound, unless the verdict was recorded already.
    pub(crate) fn set_for_subtree(&self, record: Option<&AbortRecord>) -> bool {
        self.record(SUBTREE_VERDICT, record)
    }

    fn record(&self, cause: &'static str, record: Option<&AbortRecord>) -> bool {
        let first = !self.cell.set.swap(true, Ordering::SeqCst);
        if first {
            log_verdict(self.verdicts.supervisor.get(), &self.key, cause);
            self.verdicts.notify_one();
            // Queued, not delivered: no handler runs on the path that
            // writes the verdict, so none delays the cell's publication.
            self.report_verdict(cause, record);
        }
        first
    }

    pub(crate) fn is_set(&self) -> bool {
        self.cell.set.load(Ordering::SeqCst)
    }

    /// True while a wait on the incarnation's cell is open, or once a wait
    /// was handed the verdict: a caller then holds or held the report.
    pub(crate) fn has_audience(&self) -> bool {
        self.cell.open.load(Ordering::SeqCst) > 0 || self.cell.reported.load(Ordering::SeqCst)
    }

    /// Records that a wait was handed the verdict.
    fn mark_reported(&self) {
        self.cell.reported.store(true, Ordering::SeqCst);
    }

    /// A wait on the incarnation's cell opens.
    fn open_wait(&self) {
        self.cell.open.fetch_add(1, Ordering::SeqCst);
    }

    /// A wait on the incarnation's cell ends. A verdict that no wait was
    /// handed wakes the supervisor's loop, which re-checks who is left to
    /// report it.
    fn close_wait(&self) {
        self.cell.open.fetch_sub(1, Ordering::SeqCst);
        if self.is_set() && !self.cell.reported.load(Ordering::SeqCst) {
            self.verdicts.notify_one();
        }
    }

    /// Queues the verdict event for the incarnation this flag belongs to.
    /// Called by the one writer that recorded the verdict, with no guard
    /// held. A flag without a key, or a writer without the incarnation's
    /// record, names no incarnation and reports nothing. The supervisor is the
    /// child's recorded parent, or its id read as a root.
    fn report_verdict(&self, cause: &'static str, record: Option<&AbortRecord>) {
        let (Some(record), Some((child, incarnation)), Some(supervisor)) =
            (record, self.key.as_ref(), self.verdicts.supervisor.get())
        else {
            return;
        };
        crate::telemetry::with_mode(record, None, |mode| {
            if !mode.armed(EventKind::Unresponsive) {
                return;
            }
            // The supervisor's identity, which owns the event, is the one the
            // registry holds under its id; the child's recorded parent names
            // the reference when it is there.
            let owner = mode
                .system()
                .and_then(|system| system.resolve_identity_of_id(supervisor));
            let supervisor = crate::telemetry::identity_for(mode, record, None, None)
                .and_then(|identity| identity.actor().parent())
                .or_else(|| owner.as_ref().map(|identity| identity.actor().clone()))
                .unwrap_or_else(|| {
                    ActorRef::new_root(supervisor.clone(), crate::telemetry::UNKNOWN_INCARNATION)
                });
            crate::telemetry::push_for(mode, owner.as_ref(), EventKind::Unresponsive, || {
                TelemetryEvent::Unresponsive {
                    child: ActorRef::for_child_of(&supervisor, child.clone(), *incarnation),
                    supervisor,
                    cause: std::borrow::Cow::Borrowed(cause),
                }
            });
        });
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
    /// The incarnation's abort record, taken when the predecessor is built
    /// whether or not a sink takes events then, so a sink armed afterwards
    /// still receives the Unresponsive event, which finds its system through
    /// it. `None` for a predecessor with no published incarnation.
    record: Option<AbortRecord>,
}

impl Predecessor {
    /// True once every actor of the subtree has ended.
    pub(crate) fn is_gone(&self) -> bool {
        self.subtree.is_gone()
    }

    /// The incarnation's abort record, if it was published.
    #[cfg(test)]
    pub(crate) fn record(&self) -> Option<&AbortRecord> {
        self.record.as_ref()
    }

    /// A predecessor whose verdict no supervisor reads and nothing logs.
    #[cfg(test)]
    pub(crate) fn untracked(subtree: SubtreeWatch) -> Self {
        Predecessor {
            subtree,
            verdict: VerdictFlag {
                cell: Arc::default(),
                verdicts: Arc::default(),
                key: None,
            },
            record: None,
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
        cell: Arc::default(),
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
    StopSeverity::is_vetoable(reason)
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

    /// Whether the incarnation the slot published is sensitive.
    #[cfg(feature = "tracing")]
    fn sensitive(&self) -> bool {
        self.cell_tx.borrow().lane.as_ref().is_some_and(|lane| {
            lane.abort_record().sys_bits() & crate::actor::subtree::SYS_SENSITIVE != 0
        })
    }

    /// Records that the incarnation outlived its whole ladder, and wakes the
    /// supervisor's loop so it can re-check what waits on the incarnation.
    pub(crate) fn write_verdict(&self) {
        // The shared flag first: it logs the warning, so no waiter can see the
        // verdict published before the warning exists.
        let record = self
            .cell_tx
            .borrow()
            .lane
            .as_ref()
            .map(|lane| lane.abort_record().clone());
        self.lost.set_for(record.as_ref());
        self.cell_tx.send_modify(|cell| cell.unresponsive = true);
    }

    /// Records that the attempt ended without an incarnation: a stop order
    /// or the group's cancel was honored before the attempt's factory ran,
    /// or the attempt found its child's spec removed, before or during its
    /// build.
    pub(crate) fn mark_cancelled(&self) {
        self.cell_tx.send_modify(|cell| cell.cancelled = true);
    }

    /// Records that the attempt's start timeout fired before the
    /// acknowledgment was read, and whether it raises `Kill` on its
    /// incarnation (`cut`). Written before the raise, so a reader that sees
    /// the incarnation's end sees both.
    pub(crate) fn mark_start_timeout(&self, cut: bool) {
        self.cell_tx.send_modify(|cell| {
            cell.timeout_fired = true;
            cell.timeout_cut = cut;
        });
    }

    /// Whether the attempt's start timeout raised `Kill` on its incarnation.
    fn timeout_cut(&self) -> bool {
        self.cell_tx.borrow().timeout_cut
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
    /// The incarnation's stop lane, once it was published.
    pub(crate) fn lane(&self) -> Option<StopLane> {
        self.cell_rx.borrow().lane.clone()
    }

    /// The incarnation's status plane, once it was published.
    pub(crate) fn status(&self) -> Option<StatusRx> {
        self.cell_rx.borrow().status.clone()
    }

    /// The incarnation's subtree, once it was published.
    pub(crate) fn subtree(&self) -> Option<SubtreeWatch> {
        self.cell_rx.borrow().subtree.clone()
    }

    /// `subtree`, as the subtree of this slot's incarnation.
    pub(crate) fn predecessor(&self, subtree: SubtreeWatch) -> Predecessor {
        Predecessor {
            subtree,
            verdict: self.lost.clone(),
            record: self
                .cell_rx
                .borrow()
                .lane
                .as_ref()
                .map(|lane| lane.abort_record().clone()),
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
        self.lost.open_wait();
        CellWatch {
            cell: self.cell_rx.clone(),
            lost: self.lost.clone(),
        }
    }

    /// True while some wait on the incarnation's cell is open, or once one
    /// was handed its verdict: a caller owns the report of the verdict.
    pub(crate) fn verdict_has_audience(&self) -> bool {
        self.lost.has_audience()
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

    /// Whether the cell reads the incarnation as ended now: its fate is
    /// recorded, or the status plane it holds reads `Stopped` or is closed.
    fn cell_read_ended(&self) -> bool {
        self.task_state() == TaskState::Ended || self.status_ended()
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

    /// The incarnation's verdict flag, for the tests that write the verdict
    /// by each of its routes.
    #[cfg(test)]
    pub(crate) fn lost_flag(&self) -> &VerdictFlag {
        &self.lost
    }

    /// True when a stop order was already written.
    pub(crate) fn has_order(&self) -> bool {
        self.cmd_tx.borrow().stop.is_some()
    }

    /// True when a stop order was written before the cell read the
    /// incarnation as ended: an order that could have ended it. An order
    /// written after the end did not cause it.
    pub(crate) fn order_before_end(&self) -> bool {
        self.cmd_tx
            .borrow()
            .stop
            .is_some_and(|order| !order.after_end)
    }

    /// Whether the attempt's start timeout raised `Kill` on its incarnation.
    pub(crate) fn timeout_cut(&self) -> bool {
        self.cell_rx.borrow().timeout_cut
    }

    /// Whether the attempt's start timeout fired before the attempt read
    /// the acknowledgment.
    pub(crate) fn timeout_fired(&self) -> bool {
        self.cell_rx.borrow().timeout_fired
    }

    /// Writes the stop order of an incarnation whose final reason is
    /// written. No signal is raised: its run loop reads no request any more.
    /// The order only arms its ladder, so a wait on the incarnation's end is
    /// bounded by `policy`, as the wait on any stop of it is.
    pub(crate) fn order_bound(&self, policy: Shutdown) {
        self.write_order(policy, true);
    }

    /// True when the incarnation's lane holds a request its `pre_stop` may
    /// still veto (`Graceful`, `ParentRequest` or `Shutdown`).
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
                    .map_or(StopReason::Kill, |status| status.reason_or_kill())
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
                raise_noted(
                    None,
                    &lane,
                    initial_signal(policy),
                    crate::telemetry::StopOrigin::Requested,
                );
                true
            }
            None => false,
        }
    }

    /// Writes the stop order for the incarnation. The first order wins and
    /// keeps its issue time: a second origin never restarts the clock.
    pub(crate) fn write_order(&self, policy: Shutdown, initial_raised: bool) {
        self.write_order_read_at(policy, initial_raised, self.cell_read_ended());
    }

    /// Writes the order with the ended bit read by the caller. A stop that
    /// raises the first signal reads the bit before the raise, so a child
    /// that ends in answer to that signal has an order written before its end.
    fn write_order_read_at(&self, policy: Shutdown, initial_raised: bool, after_end: bool) {
        let order = StopOrder {
            policy,
            issued_at: Instant::now(),
            initial_raised,
            after_end,
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
        let ended = self.cell_read_ended();
        let raised = raise_first && self.raise_first(policy);
        self.write_order_read_at(policy, raised, ended);
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
            raise_noted(
                None,
                &lane,
                StopReason::ParentRequest,
                crate::telemetry::StopOrigin::Requested,
            );
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
            self.lost.set_for(self.record.as_ref());
            let _ = self
                .status
                .wait_for(|value| value.status == ActorStatus::Stopped)
                .await;
        }
        if let Some(record) = &self.record {
            record.task_ended();
        }
        self.status.reason_or_kill()
    }
}

/// What a wait on a slot's cell holds: the status cell, and the
/// incarnation's verdict flag.
pub(crate) struct CellWatch {
    cell: watch::Receiver<SlotCell>,
    lost: VerdictFlag,
}

impl CellWatch {
    /// Records that the verdict this wait returned went to a caller that
    /// decides on it. Set before the wait is dropped, so the supervisor
    /// never sees a verdict with neither an open wait nor a report.
    pub(crate) fn mark_reported(&self) {
        self.lost.mark_reported();
    }
}

impl Drop for CellWatch {
    fn drop(&mut self) {
        self.lost.close_wait();
    }
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
        let reason = status.reason_or_kill();
        CellOutcome::Fate(ChildFate {
            reason,
            incarnation,
        })
    } else {
        watch.lost.set_for(record);
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
                    let fields = crate::telemetry::LogFields::of_sensitive(
                        io.is_some_and(SlotIo::sensitive),
                    );
                    caught_log!(warn,
                        supervisor = %supervisor,
                        child = %child,
                        incarnation = %incarnation,
                        reason = %fields.reason_text(&reason),
                        panic = %fields.reason_text(&map_join(Err(err))),
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
    if raise_noted(None, lane, reason, crate::telemetry::StopOrigin::Requested)
        == RaiseResult::Recorded
        && kill
    {
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
///
/// `ended` is set when the caller took the incarnation's end from its join
/// already: the task has ended with that reason and `join` is spent, and the
/// ladder takes the end as the first thing it finds, exactly as it takes the
/// end of a finished task.
pub(crate) async fn run_ladder(
    join: &mut JoinHandle<StopReason>,
    mut ended: Option<StopReason>,
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

            res = async {
                match ended.take() {
                    Some(reason) => Ok(reason),
                    None => (&mut *join).await,
                }
            } => {
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
                        predecessor
                            .verdict
                            .set_for_subtree(predecessor.record.as_ref());
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
                predecessor
                    .verdict
                    .set_for_subtree(predecessor.record.as_ref());
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

/// One restart attempt of a supervisor, counted in the supervisor's identity
/// from the moment the attempt is started until the supervisor has taken its
/// outcome. It exists only while telemetry is on for the supervisor's system,
/// so a system with telemetry off holds none.
///
/// The count falls exactly once: when the ticket is settled with the
/// attempt's outcome, or when the ticket is dropped unsettled (the supervisor
/// ended with the attempt still in its plane). A settled ticket lowers the
/// count first and then queues the attempt's event, so a reader that has seen
/// the event reads the count without the attempt.
pub(crate) struct AttemptTicket {
    flight: InFlight,
    record: AbortRecord,
    /// Read when the attempt starts, only if its event is taken then.
    born: Option<Instant>,
    /// The child and incarnation of the attempt once a supervisor stored the
    /// ticket for it: only a stored ticket reports when it drops unsettled.
    stored: Option<(ActorId, u64)>,
}

/// The count of one attempt in its supervisor's identity.
struct InFlight(Option<Arc<crate::telemetry::Identity>>);

impl InFlight {
    /// Lowers the count and hands the identity back, once.
    fn release(&mut self) -> Option<Arc<crate::telemetry::Identity>> {
        let identity = self.0.take()?;
        identity.restart_settled();
        Some(identity)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.release();
    }
}

impl AttemptTicket {
    /// Counts a new attempt of the supervisor `supervisor`, whose stop lane
    /// carries `record`, when its system takes events, holds identities or
    /// tracks turns; `None` otherwise (two relaxed loads and one more).
    pub(crate) fn begin(
        record: &AbortRecord,
        system: Option<&crate::system::ActorSystem>,
        supervisor: &ActorId,
    ) -> Option<Self> {
        let mode = match system {
            Some(system) => system.telemetry_mode(),
            None => record.mode()?,
        };
        if !mode.engaged() && mode.word() & crate::telemetry::CTL_TRACK_TURNS == 0 {
            return None;
        }
        let identity = crate::telemetry::identity_for(mode, record, system, Some(supervisor))?;
        identity.restart_started();
        Some(Self {
            flight: InFlight(Some(identity)),
            record: record.clone(),
            born: mode.armed(EventKind::RestartAttempt).then(Instant::now),
            stored: None,
        })
    }

    /// Ends the attempt that started the incarnation `incarnation` of
    /// `child`: lowers the count, then queues the attempt's event.
    pub(crate) fn settle(mut self, child: &ActorId, incarnation: u64, outcome: AttemptOutcome) {
        self.stored = None;
        self.report(child, incarnation, outcome);
    }

    /// Ends an attempt that never started an incarnation (the start was
    /// refused): lowers the count and reports nothing.
    pub(crate) fn discard(mut self) {
        self.stored = None;
        self.flight.release();
    }

    /// Lowers the count first and then queues the event, so a reader that has
    /// seen the event reads the count without the attempt. Pushes, never
    /// waits and runs no user code, so it is safe in a drop.
    fn report(&mut self, child: &ActorId, incarnation: u64, outcome: AttemptOutcome) {
        let Some(identity) = self.flight.release() else {
            return;
        };
        let born = self.born;
        crate::telemetry::with_mode(&self.record, None, |mode| {
            if !mode.armed(EventKind::RestartAttempt) {
                return;
            }
            mode.push_owned(&identity, EventKind::RestartAttempt, || {
                let supervisor = identity.actor().clone();
                TelemetryEvent::RestartAttempt {
                    child: ActorRef::for_child_of(&supervisor, child.clone(), incarnation),
                    supervisor,
                    outcome,
                    took: born.map(|born| born.elapsed()).unwrap_or_default(),
                }
            });
        });
    }
}

impl Drop for AttemptTicket {
    fn drop(&mut self) {
        // A stored ticket that is still unsettled: the supervisor ended with
        // the attempt in its plane.
        if let Some((child, incarnation)) = self.stored.take() {
            self.report(&child, incarnation, AttemptOutcome::Abandoned);
        }
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

/// The acknowledgment branch's read of the channel in a test build: a hold
/// attached at [`ACK_GAP`](crate::actor::runtime::end_hooks::ACK_GAP) parks
/// the branch before it reads, so the acknowledgment can be stored and the
/// incarnation's task end while the branch has not read, and the poll that
/// finds the task ended reads the join branch with the acknowledgment unread,
/// as a read made just before the acknowledgment was stored would leave it.
#[cfg(test)]
async fn read_ack(
    rx: &mut oneshot::Receiver<Result<(), ActorError>>,
    child_id: &ActorId,
) -> Result<Result<(), ActorError>, oneshot::error::RecvError> {
    crate::actor::runtime::end_hooks::wait_hold(
        crate::actor::runtime::end_hooks::ACK_GAP,
        child_id,
    )
    .await;
    rx.await
}

/// The executor of a restart attempt from the spawn of its incarnation: it
/// waits for the init acknowledgment, bounded by the start timeout from the
/// spawn, and honors a stop order at every moment of that wait. An order
/// starts the one ladder at once (its window from the order, the start
/// timeout still running, whichever ends first), and when the ladder ends
/// the attempt records the fate and reports the incarnation as stopped. An
/// acknowledgment read under no order hands the incarnation over, even when
/// its task already ended. A start timeout reads the channel once more
/// before it acts, so an acknowledgment stored when it expires is handled as
/// if the acknowledgment branch had read it first, and an incarnation that
/// acknowledged is not cut by it. An init error, a closed acknowledgment
/// channel and a start timeout wait for the task to end before any fate is
/// written;
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

    // The acknowledgment is read: the command cell is read once more, so an
    // order not yet read is honored before the incarnation could be handed
    // over, and an acknowledgment read under no order, before the ladder
    // acted, hands the incarnation over. `$ended` is the end the attempt
    // already took from the join, if it did.
    macro_rules! acknowledged {
        ($ended:expr) => {{
            #[cfg(test)]
            crate::actor::runtime::end_hooks::wait_hold(
                crate::actor::runtime::end_hooks::ACK,
                &child_id,
            )
            .await;
            acked = true;
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
                    ended: $ended,
                };
            }
        }};
    }

    // The init's answer is read; `$started` is whether it is an
    // acknowledgment. An acknowledgment is handled by `acknowledged`; an init
    // error and a sender dropped without a reply start the kill path, which
    // the init's own end completes.
    macro_rules! answered {
        ($started:expr) => {{
            if $started {
                acknowledged!(None)
            } else if let AttemptAction::KillPath { raise } =
                attempt_step(AttemptStage::Init, AttemptEvent::InitEnded, view!())
            {
                owe_kill_grace(&lane, raise, &join.abort_handle());
                step = kill_grace_from(Instant::now().into_std(), grace);
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

            result = async {
                let rx = ack.as_mut().expect("guarded by is_some");
                #[cfg(test)]
                let read = read_ack(rx, &child_id).await;
                #[cfg(not(test))]
                let read = rx.await;
                read
            }, if ack.is_some() => {
                ack = None;
                answered!(matches!(result, Ok(Ok(()))));
            }

            res = &mut join => {
                let reason = joined_reason(res, || io.plane_reason(), Some(&io));
                // The acknowledgment branch above may have read the channel
                // before the acknowledgment was stored, and the incarnation
                // ended since: an acknowledgment stored before the end is
                // read here, so the end is classified as it is when that
                // branch reads it first.
                if let Some(rx) = ack.as_mut() {
                    if matches!(rx.try_recv(), Ok(Ok(()))) {
                        #[cfg(test)]
                        crate::actor::runtime::end_hooks::run(
                            crate::actor::runtime::end_hooks::ACK_LATE,
                            &child_id,
                        );
                        acknowledged!(Some(reason.clone()));
                    }
                }
                advance_attempt(step, LadderEvent::TaskEnded, grace, (&join, &lane, &io));
                let outcome = ended_outcome(
                    child_id,
                    incarnation,
                    reason.clone(),
                    ordered || io.order().is_some(),
                    io.timeout_cut(),
                    view!(),
                );
                io.record_fate(ChildFate {
                    reason,
                    incarnation,
                });
                // The incarnation ended and its fate is recorded: the guard
                // has nothing left to force.
                guard.disarm();
                return outcome;
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
                // The channel is read once more before the expiry acts, so an
                // answer stored while the acknowledgment branch above was
                // polled, or a sender dropped without one, is handled as that
                // branch handles it when it reads first: an acknowledgment
                // hands the incarnation over uncut, and the rule then decides
                // what an init error or a dropped sender leaves for the
                // expiry. A channel with nothing stored and its sender alive
                // is the expiry's to cut. The receiver is cleared whenever it
                // returned anything: it must not be polled again.
                let stored = ack.as_mut().and_then(|rx| match rx.try_recv() {
                    Ok(answer) => Some(answer.is_ok()),
                    Err(oneshot::error::TryRecvError::Closed) => Some(false),
                    Err(oneshot::error::TryRecvError::Empty) => None,
                });
                if let Some(started) = stored {
                    ack = None;
                    answered!(started);
                }
                if let AttemptAction::KillPath { raise } =
                    attempt_step(AttemptStage::Init, AttemptEvent::StartTimeout, view!())
                {
                    io.mark_start_timeout(raise);
                    owe_kill_grace(&lane, raise, &join.abort_handle());
                    step = kill_grace_from(Instant::now().into_std(), grace);
                }
            }
        }
    }
}

/// The outcome of an attempt whose incarnation ended with `reason` before
/// it was handed over: `ordered` is whether a stop order was written for the
/// incarnation, `timeout_cut` whether the attempt's start timeout raised
/// `Kill` on it. The attempt itself and a supervisor that reports the
/// attempt from the incarnation's status plane both classify through here,
/// so one end gets one outcome.
pub(crate) fn ended_outcome(
    child_id: ActorId,
    incarnation: u64,
    reason: StopReason,
    ordered: bool,
    timeout_cut: bool,
    view: AttemptView,
) -> RestartOutcome {
    let event = AttemptEvent::Ended {
        ordered,
        timeout_raised: timeout_cut,
    };
    let report = match attempt_step(AttemptStage::Init, event, view) {
        AttemptAction::Report(report) => report,
        _ => AttemptReport::Failed,
    };
    match report {
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
    }
}

/// What a supervisor's final collection does with a restart attempt still in
/// its plane.
pub(crate) enum FinalAttempt {
    /// The incarnation's task published its end: the attempt is awaited.
    Await,
    /// The incarnation's start completed, it ended on its own with its final
    /// reason written, and no stop order, start timeout or cut stands: the
    /// attempt hands it over at its next poll, without waiting for the task,
    /// so it is awaited, and the wait ends early if the attempt's cell
    /// settles first (the attempt then hands nothing over).
    Adopts,
    /// The incarnation's final reason is written and the attempt hands
    /// nothing over (its start failed or was cut, or a stop order or its
    /// start timeout stands), while its task may still be dropping what it
    /// holds: the attempt is reported with this outcome, the one it reports
    /// itself, and nothing waits for the task.
    Report(Box<RestartOutcome>),
    /// The incarnation has not ended: the attempt is the teardown's. A slot
    /// with nothing published is read here too: the incarnation is published
    /// inside its spawn, before its task can run, so an unpublished slot has
    /// no incarnation that could have ended.
    Leave,
}

/// Reads, once, the status plane of the incarnation `incarnation` of
/// `child_id` whose attempt holds `ctl`, for the final collection. The plane's
/// final reason is written once and before the task drops anything, and
/// `Initializing` stays its status until `Stopped` when the start did not
/// complete, so one read decides; whichever of `Await`, `Adopts` and `Report`
/// it gives, the attempt's outcome is the same.
pub(crate) fn final_attempt(ctl: &SlotCtl, child_id: &ActorId, incarnation: u64) -> FinalAttempt {
    let Some(status) = ctl.status() else {
        return if ctl.cell_rx.has_changed().is_err() {
            FinalAttempt::Await
        } else {
            FinalAttempt::Leave
        };
    };
    let closed = status.has_changed().is_err();
    let (reason, started) = {
        let value = status.borrow();
        if closed || value.status == ActorStatus::Stopped {
            return FinalAttempt::Await;
        }
        match &value.reason {
            Some(reason) => (reason.clone(), value.status != ActorStatus::Initializing),
            None => return FinalAttempt::Leave,
        }
    };
    // A completed start is handed over unless the attempt's ladder left its
    // idle step: an order, or the start timeout, which fires only before
    // the acknowledgment is read. A failed start never is.
    if started && !ctl.has_order() && !ctl.timeout_fired() {
        return FinalAttempt::Adopts;
    }
    FinalAttempt::Report(Box::new(reported_attempt(
        ctl,
        child_id,
        incarnation,
        reason,
    )))
}

/// The outcome of the attempt of `ctl` whose incarnation's final reason
/// `reason` is written and which hands nothing over, as the attempt reports
/// it itself once the task ended.
fn reported_attempt(
    ctl: &SlotCtl,
    child_id: &ActorId,
    incarnation: u64,
    reason: StopReason,
) -> RestartOutcome {
    let view = AttemptView {
        ladder: AttemptLadder::Off,
        kill_stored: false,
        acked: false,
    };
    ended_outcome(
        child_id.clone(),
        incarnation,
        reason,
        ctl.has_order(),
        ctl.timeout_cut(),
        view,
    )
}

/// For the final collection's wait on an attempt read as
/// [`FinalAttempt::Adopts`] whose cell settled before its task returned:
/// the attempt hands nothing over, and is reported from its incarnation's
/// final reason, as it reports itself.
pub(crate) fn settled_attempt(
    ctl: &SlotCtl,
    child_id: &ActorId,
    incarnation: u64,
) -> Option<RestartOutcome> {
    let reason = ctl.status()?.borrow().reason.clone()?;
    Some(reported_attempt(ctl, child_id, incarnation, reason))
}

/// What the final collection does with a runner still in the death plane
/// whose incarnation the ledger holds.
pub(crate) enum FinalDeath {
    /// The incarnation's task published its end: the runner returns without
    /// waiting on anything the incarnation owns, so it is awaited.
    Await,
    /// The incarnation's final reason is written while its task may still be
    /// dropping what it holds: its death is collected with that reason, the
    /// one the runner returns, and nothing waits for the task.
    Report(StopReason),
    /// The incarnation has not ended: it is the teardown's.
    Leave,
}

/// Reads, once, the status plane of the incarnation whose slot is `ctl`, for
/// the final collection's read of a runner. The reason is written once,
/// before the task drops anything and after its last await, so the task
/// returns that reason and its runner returns it too.
pub(crate) fn final_death(ctl: &SlotCtl) -> FinalDeath {
    if ctl.status_ended() {
        return FinalDeath::Await;
    }
    match ctl
        .status()
        .and_then(|status| status.borrow().reason.clone())
    {
        Some(reason) => FinalDeath::Report(reason),
        None => FinalDeath::Leave,
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
    /// A stop order counts as the cause of an end only when it was written
    /// before the slot's cell read the incarnation as ended: no order, an
    /// order on a live slot, an order on a slot whose cell had closed, and an
    /// order on a slot whose status plane already read `Stopped`.
    #[test]
    fn an_order_written_after_the_end_is_not_before_it() {
        use crate::types::StatusValue;
        enum Case {
            NoOrder,
            Live,
            CellClosed,
            PlaneStopped,
        }
        for (case, want) in [
            (Case::NoOrder, false),
            (Case::Live, true),
            (Case::CellClosed, false),
            (Case::PlaneStopped, false),
        ] {
            let (ctl, io) = super::slot_pair(Default::default());
            let (lane, _rx) = StopLane::new();
            let (tx, status) = lane.abort_record().clone().status_channel();
            let own = OwnStop {
                is_supervisor: false,
                shutdown: Shutdown::Kill,
                kill_grace: Duration::ZERO,
            };
            match case {
                Case::NoOrder => {}
                Case::Live => {
                    io.publish(lane.clone(), status.clone(), own, None);
                    ctl.write_order(Shutdown::Kill, false);
                }
                Case::CellClosed => {
                    drop(io);
                    ctl.write_order(Shutdown::Kill, false);
                }
                Case::PlaneStopped => {
                    io.publish(lane.clone(), status.clone(), own, None);
                    tx.send_modify(|value: &mut StatusValue| value.status = ActorStatus::Stopped);
                    ctl.write_order(Shutdown::Kill, false);
                }
            }
            assert_eq!(ctl.order_before_end(), want);
        }
    }

    /// A child that ends in answer to the first signal of a stop was ordered
    /// before its end: the ended bit is read before the raise, so the signal's
    /// own effect on the status plane does not turn the order into one
    /// written after the end.
    #[test]
    fn an_order_whose_own_raise_ends_the_child_is_before_the_end() {
        use crate::types::StatusValue;
        use std::future::Future;
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};

        struct EndsTheChild(crate::types::status_cell::StatusTx);
        impl Wake for EndsTheChild {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0
                    .send_modify(|value: &mut StatusValue| value.status = ActorStatus::Stopped);
            }
        }

        let (ctl, io) = super::slot_pair(Default::default());
        let (lane, mut rx) = StopLane::new();
        let (tx, status) = lane.abort_record().clone().status_channel();
        let own = OwnStop {
            is_supervisor: false,
            shutdown: Shutdown::Kill,
            kill_grace: Duration::ZERO,
        };
        io.publish(lane.clone(), status, own, None);
        // The child's stand-in is woken by the raise and ends at once.
        let waker = Waker::from(Arc::new(EndsTheChild(tx)));
        let mut cx = Context::from_waker(&waker);
        let mut changed = Box::pin(rx.changed());
        assert!(matches!(changed.as_mut().poll(&mut cx), Poll::Pending));
        ctl.order_stop(Shutdown::Kill, true);
        assert!(ctl.status_ended(), "the raise ended the child");
        assert!(ctl.order_before_end());
    }

    /// The veto check reads the request tier: `Graceful`, `ParentRequest`
    /// and `Shutdown` may be vetoed by `pre_stop`, every other reason may not.
    #[test]
    fn only_a_requested_reason_is_vetoable() {
        use crate::error::ActorError;
        for (reason, want) in [
            (StopReason::Graceful, true),
            (StopReason::ParentRequest, true),
            (StopReason::Shutdown("x".into()), true),
            (StopReason::Failure(ActorError::user("f")), false),
            (StopReason::ParentKilled, false),
            (StopReason::Cancelled, false),
            (StopReason::Kill, false),
        ] {
            assert_eq!(super::vetoable(&reason), want, "{reason:?}");
        }
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use std::ops::Not as _;

    use tokio::sync::{mpsc, oneshot};
    use tokio::task::JoinSet;

    use super::*;
    use crate::actor::runtime::spawn_runner;
    use crate::actor::subtree::{SubtreeHold, SubtreeLink};
    use crate::actor::supervision::{
        ChildLifecycle, ChildLinkGuard, ChildSpec, ChildState, DeathOutcome, Origin,
        SupervisionConfig, SupervisionState, TaskPlane, ABORT_SETTLE,
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

    struct Rig {
        death_set: JoinSet<DeathOutcome>,
        ctl: SlotCtl,
        abort: tokio::task::AbortHandle,
        child: ChildStats,
        _reaper: mpsc::UnboundedReceiver<crate::system::ReaperEntry>,
    }

    struct ChildStats {
        started: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
        vetoed: Arc<AtomicUsize>,
        lane: StopLane,
    }

    /// A runner over a vetoing child, its order not yet written.
    fn rig(turn: Duration, hang: bool, grace: Duration) -> Rig {
        let child = spawn_vetoing_child(turn, hang);
        let (ctl, io) = slot_pair(Default::default());
        io.publish(
            child.lane.clone(),
            child.status.clone(),
            OwnStop::worker(DEFAULT_KILL_GRACE),
            None,
        );
        rig_over(child, ctl, io, grace)
    }

    fn rig_over(child: Child, ctl: SlotCtl, io: SlotIo, grace: Duration) -> Rig {
        let abort = child.join.abort_handle();
        let (reaper_tx, reaper) = mpsc::unbounded_channel();
        let guard = ChildLinkGuard::new(
            child.lane.clone(),
            abort.clone(),
            reaper_tx,
            crate::actor::supervision::rules::InheritedStop::Kill {
                grace: DEFAULT_KILL_GRACE,
            },
            dummy_system(),
            tokio::runtime::Handle::current(),
        );
        let mut death_set: JoinSet<DeathOutcome> = JoinSet::new();
        spawn_runner(
            &mut death_set,
            ActorId::from("slot-child"),
            1,
            child.join,
            None,
            child.lane.clone(),
            io,
            guard,
            grace,
            None,
        );
        Rig {
            death_set,
            ctl,
            abort,
            child: ChildStats {
                started: child.started,
                finished: child.finished,
                vetoed: child.vetoed,
                lane: child.lane,
            },
            _reaper: reaper,
        }
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

    /// The upper bound of a timed outcome whose exact instant is `exact`: on
    /// the paused clock the outcome lands on it, within one turn; on real
    /// time what the host adds is not the test's to bound, so only a hang
    /// fails.
    fn upper(paused: bool, exact: Duration) -> Duration {
        if paused {
            exact + Duration::from_millis(60)
        } else {
            BOUND
        }
    }

    // - the shared bodies ------------------------------------------------------

    async fn runner_escalates_a_vetoing_child_after_its_caller_drops_the_wait(paused: bool) {
        // A kill grace far longer than one turn: the child reaches its next
        // turn boundary inside it however late the host runs the test, so it
        // ends with the `Kill` it observed there, never by the abort.
        let mut rig = rig(ONE_TURN, false, Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(12)).await;
        let t0 = Instant::now();
        let window = Duration::from_millis(200);
        // The caller commits and then drops its wait almost at once.
        rig.ctl.order_stop(Shutdown::Timeout(window), true);
        let mut cell = rig.ctl.watch();
        let waited = timeout(Duration::from_millis(20), await_cell(&mut cell, 1)).await;
        assert!(waited.is_err(), "the caller's wait was dropped");
        let (_, reason, incarnation) = bounded(rig.death_set.join_next()).await.unwrap().unwrap();
        let took = t0.elapsed();
        assert!(matches!(reason, StopReason::Kill), "{reason:?}");
        assert_eq!(incarnation, 1);
        assert!(took >= window, "the window was honored: {took:?}");
        assert!(
            took < upper(paused, window + DEFAULT_KILL_GRACE),
            "the Kill landed within one turn of the window: {took:?}"
        );
        assert_eq!(
            rig.child.started.load(Ordering::SeqCst),
            rig.child.finished.load(Ordering::SeqCst),
            "the Kill was observed only at a turn boundary"
        );
        assert!(matches!(
            rig.ctl.fate().map(|fate| fate.reason),
            Some(StopReason::Kill)
        ));
    }

    async fn runner_aborts_a_turn_parked_at_an_await_after_kill_is_ignored(_paused: bool) {
        let mut rig = rig(ONE_TURN, true, DEFAULT_KILL_GRACE);
        // The turn is parked before the order is written: a host that runs
        // the child late would otherwise let it read the `Kill` between turns
        // and end by itself. The wait only limits how long a real hang takes
        // to report.
        timeout(Duration::from_secs(60), async {
            while rig.child.started.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the child begins its turn");
        let t0 = Instant::now();
        rig.ctl
            .order_stop(Shutdown::Timeout(Duration::from_millis(50)), true);
        let mut cell = rig.ctl.watch();
        match bounded(await_cell(&mut cell, 1)).await {
            CellOutcome::Fate(fate) => assert!(matches!(fate.reason, StopReason::Kill)),
            other => panic!("expected the aborted child's fate, got {other:?}"),
        }
        let took = t0.elapsed();
        assert!(
            took >= Duration::from_millis(50) + DEFAULT_KILL_GRACE,
            "Timeout, then Kill (ignored mid-turn), then the abort: {took:?}"
        );
        let (_, reason, _) = bounded(rig.death_set.join_next()).await.unwrap().unwrap();
        assert!(matches!(reason, StopReason::Kill));
        assert!(
            !rig.ctl.is_unresponsive(),
            "an abort that landed is no verdict"
        );
    }

    async fn runner_honors_a_stop_order_written_before_it_started(_paused: bool) {
        // The order predates the runner and its first signal was never
        // raised: no lane existed when it was written.
        let child = spawn_vetoing_child(Duration::from_millis(10), false);
        let (ctl, io) = slot_pair(Default::default());
        ctl.write_order(Shutdown::Timeout(Duration::from_millis(80)), false);
        io.publish(
            child.lane.clone(),
            child.status.clone(),
            OwnStop::worker(DEFAULT_KILL_GRACE),
            None,
        );
        let mut rig = rig_over(child, ctl, io, DEFAULT_KILL_GRACE);
        let mut cell = rig.ctl.watch();
        match bounded(await_cell(&mut cell, 1)).await {
            CellOutcome::Fate(fate) => assert!(matches!(fate.reason, StopReason::Kill)),
            other => panic!("expected the child's fate, got {other:?}"),
        }
        assert!(
            rig.child.vetoed.load(Ordering::SeqCst) >= 1,
            "the runner raised the first signal itself"
        );
        let _ = bounded(rig.death_set.join_next()).await;
    }

    async fn runner_with_kill_policy_skips_the_graceful_window(paused: bool) {
        let mut rig = rig(ONE_TURN, false, DEFAULT_KILL_GRACE);
        tokio::time::sleep(Duration::from_millis(12)).await;
        let t0 = Instant::now();
        rig.ctl.order_stop(Shutdown::Kill, true);
        let (_, reason, _) = bounded(rig.death_set.join_next()).await.unwrap().unwrap();
        assert!(matches!(reason, StopReason::Kill));
        // The `Kill` is observed at the next turn boundary, well inside a
        // window of 400 ms on the paused clock.
        let no_window = if paused {
            Duration::from_millis(400)
        } else {
            BOUND
        };
        assert!(
            t0.elapsed() < no_window,
            "no graceful window: {:?}",
            t0.elapsed()
        );
        assert_eq!(
            rig.child.vetoed.load(Ordering::SeqCst),
            0,
            "the child was never asked politely"
        );
    }

    async fn runner_with_infinity_policy_never_escalates(paused: bool) {
        let mut rig = rig(Duration::from_millis(10), false, DEFAULT_KILL_GRACE);
        tokio::time::sleep(Duration::from_millis(12)).await;
        rig.ctl.order_stop(Shutdown::Infinity, true);
        // Paused time lets an hour pass; a real clock lets a few windows pass.
        let idle = if paused {
            Duration::from_secs(3600)
        } else {
            Duration::from_millis(400)
        };
        tokio::time::sleep(idle).await;
        assert!(
            matches!(rig.child.lane.reason(), Some(StopReason::ParentRequest)),
            "the lane still reads the polite request"
        );
        assert!(!rig.abort.is_finished(), "the child still runs");
        assert!(rig.ctl.fate().is_none());
        assert!(rig.child.vetoed.load(Ordering::SeqCst) >= 1);
        // The child obeys at last: the fate is recorded.
        rig.child.lane.raise(StopReason::Kill);
        let (_, reason, _) = bounded(rig.death_set.join_next()).await.unwrap().unwrap();
        assert!(matches!(reason, StopReason::Kill));
        assert!(rig.ctl.fate().is_some());
    }

    async fn runner_starts_the_clock_at_its_own_raise_for_an_order_it_raises(_paused: bool) {
        // An order whose first signal was not raised, written long before the
        // runner starts: the window runs from the runner's raise.
        let child = spawn_vetoing_child(Duration::from_millis(10), false);
        let (ctl, io) = slot_pair(Default::default());
        ctl.write_order(Shutdown::Timeout(Duration::from_millis(200)), false);
        tokio::time::sleep(Duration::from_millis(500)).await;
        io.publish(
            child.lane.clone(),
            child.status.clone(),
            OwnStop::worker(DEFAULT_KILL_GRACE),
            None,
        );
        let started = Instant::now();
        let mut rig = rig_over(child, ctl, io, DEFAULT_KILL_GRACE);
        let (_, reason, _) = bounded(rig.death_set.join_next()).await.unwrap().unwrap();
        assert!(matches!(reason, StopReason::Kill));
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "the full window ran after the runner's raise: {:?}",
            started.elapsed()
        );
    }

    async fn ladder_waits_the_grace_it_is_given_between_the_kill_and_the_abort(_paused: bool) {
        for grace in [
            Duration::ZERO,
            Duration::from_millis(100),
            Duration::from_millis(300),
        ] {
            // A child parked at an await that never reaches a turn boundary.
            let child = tokio::spawn(std::future::pending::<StopReason>());
            let probe = child.abort_handle();
            let (lane, _lane_rx) = StopLane::new();
            let (ctl, io) = slot_pair(Default::default());
            ctl.write_order(Shutdown::Kill, false);
            let mut join = child;
            let mut io = io;
            let ladder = tokio::spawn({
                let lane = lane.clone();
                async move { run_ladder(&mut join, None, &lane, &mut io, grace).await }
            });
            // The Kill is raised at once; the abort follows after `grace`.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            assert!(matches!(lane.reason(), Some(StopReason::Kill)));
            if grace.is_zero() {
                tokio::time::sleep(Duration::from_millis(1)).await;
                assert!(probe.is_finished(), "a zero grace aborts at the Kill");
            } else {
                tokio::time::sleep(grace - Duration::from_millis(10)).await;
                assert!(!probe.is_finished(), "never before the grace has passed");
                tokio::time::sleep(Duration::from_millis(20)).await;
                assert!(probe.is_finished(), "the abort lands after the grace");
            }
            let reason = bounded(ladder).await.unwrap();
            assert!(matches!(reason, StopReason::Kill), "{reason:?}");
            drop(ctl);
        }
    }

    async fn await_cell_waits_on_a_closed_cells_status_until_its_incarnation_ends(_paused: bool) {
        // A cell closed over a live incarnation returns nothing until the
        // status it held reads `Stopped`.
        let (ctl, io) = slot_pair(Default::default());
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _lane_rx) = StopLane::new();
        io.publish(lane, status, OwnStop::worker(DEFAULT_KILL_GRACE), None);
        drop(io);
        let mut cell = ctl.watch();
        let waiter = tokio::spawn(async move { await_cell(&mut cell, 9).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "a closed cell settles nothing by itself"
        );
        status_tx.send_replace(StatusValue::new(ActorStatus::Stopped));
        match bounded(waiter).await.unwrap() {
            CellOutcome::Fate(fate) => {
                assert!(matches!(fate.reason, StopReason::Kill));
                assert_eq!(fate.incarnation, 9);
            }
            other => panic!("expected the incarnation's end, got {other:?}"),
        }

        // The status sender dropping, an abort, ends it too.
        let (ctl, io) = slot_pair(Default::default());
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _lane_rx) = StopLane::new();
        io.publish(lane, status, OwnStop::worker(DEFAULT_KILL_GRACE), None);
        drop(io);
        let mut cell = ctl.watch();
        let waiter = tokio::spawn(async move { await_cell(&mut cell, 4).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(status_tx);
        assert!(matches!(
            bounded(waiter).await.unwrap(),
            CellOutcome::Fate(ChildFate {
                reason: StopReason::Kill,
                incarnation: 4
            })
        ));

        // An incarnation that outlives its stop budget gets the verdict, and
        // only after that budget.
        let (ctl, io) = slot_pair(Default::default());
        let (_status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _lane_rx) = StopLane::new();
        io.publish(lane, status, OwnStop::worker(DEFAULT_KILL_GRACE), None);
        drop(io);
        let mut cell = ctl.watch();
        let t0 = Instant::now();
        let outcome = await_cell(&mut cell, 3).await;
        assert!(matches!(outcome, CellOutcome::Unresponsive), "{outcome:?}");
        assert!(
            t0.elapsed() >= DEFAULT_KILL_GRACE + ABORT_SETTLE,
            "{:?}",
            t0.elapsed()
        );

        // A cell closed before its incarnation existed returns at once.
        let (ctl, io) = slot_pair(Default::default());
        drop(io);
        let mut cell = ctl.watch();
        let outcome = timeout(Duration::from_millis(10), await_cell(&mut cell, 2))
            .await
            .expect("returns at once");
        assert!(matches!(outcome, CellOutcome::NoIncarnation), "{outcome:?}");
    }

    /// How far before the bound a wait is checked to still be waiting: exact
    /// under paused time, a margin a loaded machine cannot cross otherwise.
    fn early(paused: bool) -> Duration {
        if paused {
            Duration::from_millis(10)
        } else {
            Duration::from_millis(100)
        }
    }

    /// Waits on `cell` in a task of its own and returns its outcome with the
    /// instant the wait returned, read in that task: never earlier than the
    /// verdict, so a check that it comes no sooner than its bound holds on a
    /// host that runs the test late.
    fn timed_cell(
        mut cell: CellWatch,
        incarnation: u64,
    ) -> tokio::task::JoinHandle<(CellOutcome, Instant)> {
        tokio::spawn(async move {
            let outcome = await_cell(&mut cell, incarnation).await;
            (outcome, Instant::now())
        })
    }

    async fn closed_cell_waits_the_incarnations_own_kill_grace(paused: bool) {
        let grace = Duration::from_millis(600);
        // A status that never reaches `Stopped`: the verdict comes at the
        // incarnation's own grace plus the settle time, and not before.
        let (ctl, io) = slot_pair(Default::default());
        let (_status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _lane_rx) = StopLane::new();
        io.publish(lane, status, OwnStop::worker(grace), None);
        drop(io);
        let started = Instant::now();
        let waiter = timed_cell(ctl.watch(), 3);
        let bound = grace + ABORT_SETTLE;
        let (outcome, at) = bounded(waiter).await.unwrap();
        assert!(matches!(outcome, CellOutcome::Unresponsive), "{outcome:?}");
        let at = at - started;
        assert!(
            at >= bound,
            "the wait lasts the incarnation's own grace plus the settle time: {at:?}"
        );
        if paused {
            assert_eq!(at, bound, "the verdict at the bound");
        }

        // A zero grace is the settle time, never at once.
        let (ctl, io) = slot_pair(Default::default());
        let (_status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _lane_rx) = StopLane::new();
        io.publish(lane, status, OwnStop::worker(Duration::ZERO), None);
        drop(io);
        let started = Instant::now();
        let waiter = timed_cell(ctl.watch(), 4);
        let (outcome, at) = bounded(waiter).await.unwrap();
        assert!(matches!(outcome, CellOutcome::Unresponsive), "{outcome:?}");
        let at = at - started;
        assert!(
            at >= ABORT_SETTLE,
            "a zero grace still waits the settle time: {at:?}"
        );
        if paused {
            assert_eq!(at, ABORT_SETTLE, "the verdict at the settle time");
        }

        // A status that reaches `Stopped` inside the bound is the incarnation's
        // end, reported as the `Kill` it ended with.
        let (ctl, io) = slot_pair(Default::default());
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _lane_rx) = StopLane::new();
        io.publish(lane, status, OwnStop::worker(grace), None);
        drop(io);
        let mut cell = ctl.watch();
        let waiter = tokio::spawn(async move { await_cell(&mut cell, 5).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        status_tx.send_replace(StatusValue::new(ActorStatus::Stopped));
        match bounded(waiter).await.unwrap() {
            CellOutcome::Fate(fate) => {
                assert!(matches!(fate.reason, StopReason::Kill));
                assert_eq!(fate.incarnation, 5);
            }
            other => panic!("expected the incarnation's end, got {other:?}"),
        }
    }

    async fn lost_end_waits_the_incarnations_own_kill_grace(paused: bool) {
        let grace = Duration::from_millis(600);
        let verdicts: Arc<Verdicts> = Arc::default();
        let (ctl, io) = slot_pair(verdicts.clone());
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (lane, _lane_rx) = StopLane::new();
        io.publish(lane, status, OwnStop::worker(grace), None);
        let end = ctl.lost_end().expect("an incarnation was published");
        let bound = grace + ABORT_SETTLE;
        let start = Instant::now();
        let waiter = tokio::spawn(end.wait());
        // The verdict wakes the supervisor right after it is recorded, so the
        // instant the wake-up is seen is never earlier than the verdict: it
        // comes no sooner than the incarnation's own grace plus the settle
        // time, and exactly then under paused time.
        bounded(verdicts.notified()).await;
        let at = start.elapsed();
        assert!(at >= bound, "the supervisor is not woken early: {at:?}");
        if paused {
            assert!(at < bound + early(paused), "woken at the bound: {at:?}");
        }
        assert!(ctl.is_unresponsive(), "the verdict is recorded");
        assert!(
            timeout(Duration::from_millis(100), verdicts.notified())
                .await
                .is_err(),
            "once"
        );
        // The wait goes on, and returns `Kill` once the status reads `Stopped`.
        assert!(!waiter.is_finished());
        status_tx.send_replace(StatusValue::new(ActorStatus::Stopped));
        assert!(matches!(bounded(waiter).await.unwrap(), StopReason::Kill));
    }

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

    async fn closed_cell_follows_a_supervisor_childs_window_and_policy(paused: bool) {
        let window = Duration::from_millis(400);
        let grace = Duration::from_millis(100);
        let rows = [
            // A supervisor child under a timeout: its window, then its grace
            // and the settle time.
            (
                child_of(true, Shutdown::Timeout(window), grace),
                window + grace + ABORT_SETTLE,
            ),
            // Under `Kill` the window is nothing.
            (
                child_of(true, Shutdown::Kill, Duration::from_millis(300)),
                Duration::from_millis(300) + ABORT_SETTLE,
            ),
            // A worker's own policy never counts.
            (
                child_of(false, Shutdown::Timeout(window), grace),
                grace + ABORT_SETTLE,
            ),
            (
                child_of(false, Shutdown::Infinity, grace),
                grace + ABORT_SETTLE,
            ),
        ];
        for (n, (own, bound)) in rows.into_iter().enumerate() {
            let (ctl, _status_tx, _verdicts) = lost_incarnation(own);
            let started = Instant::now();
            let waiter = timed_cell(ctl.watch(), n as u64);
            let (outcome, at) = bounded(waiter).await.unwrap();
            assert!(
                matches!(outcome, CellOutcome::Unresponsive),
                "row {n}: {outcome:?}"
            );
            let at = at - started;
            assert!(at >= bound, "row {n}: the wait lasts {bound:?}: {at:?}");
            if paused {
                assert_eq!(at, bound, "row {n}: the verdict at the bound");
            }
            assert!(ctl.is_unresponsive(), "row {n}");
        }

        // Under `Infinity` no verdict is written however long the orderly
        // stop takes, and the wait ends with the incarnation's end.
        let (ctl, status_tx, _verdicts) =
            lost_incarnation(child_of(true, Shutdown::Infinity, grace));
        let mut cell = ctl.watch();
        let waiter = tokio::spawn(async move { await_cell(&mut cell, 9).await });
        // Three times the bound a worker would get.
        tokio::time::sleep((grace + ABORT_SETTLE) * 3).await;
        assert!(!waiter.is_finished(), "an unbounded stop is waited for");
        assert!(!ctl.is_unresponsive(), "no verdict is written");
        status_tx.send_replace(ended_with(StopReason::ParentKilled));
        match bounded(waiter).await.unwrap() {
            CellOutcome::Fate(fate) => {
                assert!(matches!(fate.reason, StopReason::ParentKilled));
                assert_eq!(fate.incarnation, 9);
            }
            other => panic!("expected the incarnation's end, got {other:?}"),
        }
        assert!(!ctl.is_unresponsive());

        // A grace that saturates leaves the supervisor child a bound of
        // thirty years, never a short one.
        if paused {
            let year = Duration::from_secs(86_400 * 365);
            let (ctl, _status_tx, _verdicts) =
                lost_incarnation(child_of(true, Shutdown::Timeout(window), Duration::MAX));
            let mut cell = ctl.watch();
            let waiter = tokio::spawn(async move { await_cell(&mut cell, 10).await });
            tokio::task::yield_now().await;
            tokio::time::advance(year * 29).await;
            tokio::task::yield_now().await;
            assert!(!waiter.is_finished());
            tokio::time::advance(year * 2).await;
            let outcome = bounded(waiter).await.unwrap();
            assert!(matches!(outcome, CellOutcome::Unresponsive), "{outcome:?}");
        }
    }

    async fn lost_end_follows_a_supervisor_childs_window_and_policy(paused: bool) {
        let window = Duration::from_millis(400);
        let grace = Duration::from_millis(100);
        let rows = [
            (
                child_of(true, Shutdown::Timeout(window), grace),
                window + grace + ABORT_SETTLE,
            ),
            (
                child_of(true, Shutdown::Kill, Duration::from_millis(300)),
                Duration::from_millis(300) + ABORT_SETTLE,
            ),
            (
                child_of(false, Shutdown::Timeout(window), grace),
                grace + ABORT_SETTLE,
            ),
            (
                child_of(false, Shutdown::Infinity, grace),
                grace + ABORT_SETTLE,
            ),
        ];
        for (n, (own, bound)) in rows.into_iter().enumerate() {
            let (ctl, status_tx, verdicts) = lost_incarnation(own);
            let end = ctl.lost_end().expect("an incarnation was published");
            let start = Instant::now();
            let waiter = tokio::spawn(end.wait());
            // The wake-up is seen no earlier than the verdict it follows.
            bounded(verdicts.notified()).await;
            let at = start.elapsed();
            assert!(at >= bound, "row {n}: no verdict before {bound:?}: {at:?}");
            if paused {
                assert!(
                    at < bound + early(paused),
                    "row {n}: the verdict at the bound: {at:?}"
                );
            }
            assert!(ctl.is_unresponsive(), "row {n}: the verdict is recorded");
            assert!(!waiter.is_finished(), "row {n}: the wait goes on");
            status_tx.send_replace(StatusValue::new(ActorStatus::Stopped));
            assert!(matches!(bounded(waiter).await.unwrap(), StopReason::Kill));
        }

        // Under `Infinity` the wait has no bound: no verdict, no wake-up.
        let (ctl, status_tx, verdicts) =
            lost_incarnation(child_of(true, Shutdown::Infinity, grace));
        let end = ctl.lost_end().expect("an incarnation was published");
        let waiter = tokio::spawn(end.wait());
        tokio::time::sleep((grace + ABORT_SETTLE) * 3).await;
        assert!(!ctl.is_unresponsive(), "no verdict is written");
        assert!(
            timeout(Duration::from_millis(1), verdicts.notified())
                .await
                .is_err(),
            "the supervisor is not woken"
        );
        assert!(!waiter.is_finished());
        status_tx.send_replace(ended_with(StopReason::ParentKilled));
        assert!(matches!(
            bounded(waiter).await.unwrap(),
            StopReason::ParentKilled
        ));
        assert!(!ctl.is_unresponsive());

        if paused {
            let year = Duration::from_secs(86_400 * 365);
            let (ctl, _status_tx, _verdicts) =
                lost_incarnation(child_of(true, Shutdown::Timeout(window), Duration::MAX));
            let end = ctl.lost_end().expect("an incarnation was published");
            let waiter = tokio::spawn(end.wait());
            tokio::task::yield_now().await;
            tokio::time::advance(year * 29).await;
            tokio::task::yield_now().await;
            assert!(!ctl.is_unresponsive());
            tokio::time::advance(year * 2).await;
            tokio::task::yield_now().await;
            assert!(ctl.is_unresponsive());
            waiter.abort();
        }
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

    async fn first_stop_order_keeps_its_issue_time(_paused: bool) {
        let (ctl, io) = slot_pair(Default::default());
        let (lane, lane_rx) = StopLane::new();
        let (_status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        io.publish(lane, status, OwnStop::worker(DEFAULT_KILL_GRACE), None);
        ctl.order_stop(Shutdown::Timeout(Duration::from_secs(5)), true);
        let first = io.cmd_rx.borrow().stop.expect("the first order is written");
        assert!(first.initial_raised);
        let generation = lane_rx.borrow().generation;
        tokio::time::sleep(Duration::from_millis(50)).await;
        // A second origin neither restarts the clock nor raises again.
        ctl.order_stop(Shutdown::Timeout(Duration::from_secs(1)), true);
        ctl.order_stop(Shutdown::Kill, true);
        let second = io.cmd_rx.borrow().stop.expect("the order stays");
        assert_eq!(second.issued_at, first.issued_at);
        assert!(matches!(second.policy, Shutdown::Timeout(d) if d == Duration::from_secs(5)));
        assert_eq!(lane_rx.borrow().generation, generation);
    }

    async fn runner_flags_unresponsive_and_still_records_a_late_fate() {
        // A turn that never yields, held by a gate the test opens after it has
        // seen the verdict: the abort cannot land until the turn ends, so the
        // verdict is written first. The test orders the stop only once the
        // turn has begun (an abort before the task's first poll would end it
        // with no turn to outlive), and the turn ends only when the test says
        // so (a turn that ended on its own clock could end before a starved
        // runner finished its ladder). The gate opens on every path: a
        // dropped sender ends the wait.
        let (lane, mut lane_rx) = StopLane::new();
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let turn_ended = Arc::new(AtomicUsize::new(0));
        let ended = turn_ended.clone();
        let (entered_tx, entered_rx) = oneshot::channel::<()>();
        let (open_gate, gate) = std::sync::mpsc::channel::<()>();
        let join: JoinHandle<StopReason> = tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _ = entered_tx.send(());
            let _ = gate.recv();
            ended.store(1, Ordering::SeqCst);
            loop {
                if lane_rx.changed().await.is_err() {
                    return StopReason::Kill;
                }
                if matches!(lane_rx.borrow_and_update().reason, Some(StopReason::Kill)) {
                    status_tx.send_replace(StatusValue::new(ActorStatus::Stopped));
                    return StopReason::Kill;
                }
            }
        });
        let (ctl, io) = slot_pair(Default::default());
        io.publish(
            lane.clone(),
            status,
            OwnStop::worker(DEFAULT_KILL_GRACE),
            None,
        );
        let child = Child {
            join,
            lane,
            status: StatusTx::channel(StatusValue::new(ActorStatus::Running)).1,
            started: Arc::new(AtomicUsize::new(0)),
            finished: Arc::new(AtomicUsize::new(0)),
            vetoed: Arc::new(AtomicUsize::new(0)),
        };
        let mut rig = rig_over(child, ctl, io, DEFAULT_KILL_GRACE);
        bounded(entered_rx).await.expect("the turn began");
        rig.ctl
            .order_stop(Shutdown::Timeout(Duration::from_millis(50)), true);
        let mut cell = rig.ctl.watch();
        match bounded(await_cell(&mut cell, 1)).await {
            CellOutcome::Unresponsive => {}
            other => panic!("expected the verdict, got {other:?}"),
        }
        assert_eq!(
            turn_ended.load(Ordering::SeqCst),
            0,
            "the flag was set before the turn ended"
        );
        assert!(rig.ctl.fate().is_none());
        // After the turn the abort lands and the fate is recorded.
        open_gate.send(()).expect("the turn waits at its gate");
        let (_, reason, _) = bounded(rig.death_set.join_next()).await.unwrap().unwrap();
        assert!(matches!(reason, StopReason::Kill));
        assert!(matches!(
            rig.ctl.fate().map(|fate| fate.reason),
            Some(StopReason::Kill)
        ));
        assert!(rig.ctl.is_unresponsive(), "the flag stays beside the fate");
    }

    // - each shared body on both runtime flavors -------------------------------

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
                significant: false,
                sys: crate::actor::sys::SysOptions::default(),
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

    async fn runner_join_error_before_any_spawn_is_reported_at_once() {
        // The slot held no incarnation: nothing can follow the attempt's end,
        // so its failure is reported at once, in both planes.
        let (ctl, io) = slot_pair(Default::default());
        let mut sup = state_with_a_child("early", ctl, 6);
        let task = sup.death_set.spawn(async move {
            let _io = io;
            tokio::task::yield_now().await;
            if std::hint::black_box(true) {
                panic!("attempt failed");
            }
            (ActorId::from("early"), StopReason::Graceful, 6)
        });
        sup.task_owner
            .insert(task.id(), (ActorId::from("early"), 6, TaskPlane::Death));
        let err = sup
            .death_set
            .join_next_with_id()
            .await
            .unwrap()
            .expect_err("the task failed");
        let (panic, _) = sup.lost_death_task(err);
        assert!(matches!(&panic, Some(ActorError::Panic(text)) if text == "attempt failed"));
        let (id, reason, incarnation) =
            timeout(Duration::from_millis(50), sup.death_set.join_next())
                .await
                .expect("reported at once")
                .unwrap()
                .unwrap();
        assert_eq!((id.as_str(), incarnation), ("early", 6));
        assert!(
            matches!(&reason, StopReason::Failure(ActorError::Panic(text)) if text == "attempt failed"),
            "{reason:?}"
        );

        // The restart plane: an attempt that failed before its incarnation
        // existed is routed as a failed attempt.
        let (ctl, io) = slot_pair(Default::default());
        let mut sup = state_with_a_child("attempt", ctl, 7);
        if let Some(child) = sup.registry.get_mut(&ActorId::from("attempt")) {
            child.lifecycle = ChildLifecycle::Restarting {
                incarnation: 7,
                next: 8,
            };
        }
        let task = sup.restart_set.spawn(async move {
            let _io = io;
            tokio::task::yield_now().await;
            if std::hint::black_box(true) {
                panic!("attempt task failed");
            }
            crate::actor::supervision::RestartOutcome::Failed {
                child_id: ActorId::from("attempt"),
                incarnation: 8,
                reason: StopReason::Graceful,
            }
        });
        sup.task_owner
            .insert(task.id(), (ActorId::from("attempt"), 8, TaskPlane::Restart));
        let Err(err) = bounded(sup.restart_set.join_next_with_id()).await.unwrap() else {
            panic!("the attempt task failed");
        };
        let (outcome, panic, _) = sup.lost_restart_task(err);
        assert!(matches!(&panic, Some(ActorError::Panic(text)) if text == "attempt task failed"));
        match outcome {
            Some(crate::actor::supervision::RestartOutcome::Failed {
                child_id,
                incarnation,
                reason: StopReason::Failure(ActorError::Panic(text)),
            }) => {
                assert_eq!((child_id.as_str(), incarnation), ("attempt", 8));
                assert_eq!(text, "attempt task failed");
            }
            _ => panic!("expected the attempt to be reported failed at once"),
        }
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

    // - restart attempts ----------------------------------------------------------

    struct AttemptRig {
        ctl: SlotCtl,
        run: tokio::task::JoinHandle<RestartOutcome>,
        child: ChildStats,
        verdicts: Arc<Verdicts>,
        /// The init acknowledgment's sender: kept open so the attempt's init
        /// never ends, unless a test takes it and sends.
        ack: Option<oneshot::Sender<Result<(), ActorError>>>,
        _reaper: mpsc::UnboundedReceiver<crate::system::ReaperEntry>,
    }

    /// An attempt over `child`, whose init never acknowledges.
    fn attempt_rig(child: Child, start_timeout: Option<Duration>, grace: Duration) -> AttemptRig {
        attempt_rig_as("attempt-child", child, start_timeout, grace)
    }

    /// The same rig under the child id `id`: a test that parks the attempt at
    /// a hook names its own id, since hooks are keyed by id and the tests run
    /// side by side.
    fn attempt_rig_as(
        id: &str,
        child: Child,
        start_timeout: Option<Duration>,
        grace: Duration,
    ) -> AttemptRig {
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
            child_id: ActorId::from(id),
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
            ack: Some(ack_tx),
            _reaper: reaper,
        }
    }

    /// The two ends of a stuck child's gate: `entered` resolves once its turn
    /// has begun, and `open` ends the turn. The turn also ends when `open`
    /// drops, so a test that fails first does not leave a worker blocked.
    struct Turn {
        entered: oneshot::Receiver<()>,
        open: std::sync::mpsc::Sender<()>,
    }

    /// A child whose single turn blocks its thread until the test opens its
    /// gate, then ends on a `Kill`. A test orders the stop only after the
    /// turn has begun (an abort before the task's first poll ends it with no
    /// turn to outlive) and opens the gate only after it has seen what it
    /// asserts while the turn runs, so no clock decides which comes first.
    fn stuck_child() -> (Child, Turn) {
        let (lane, mut lane_rx) = StopLane::new();
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (entered_tx, entered) = oneshot::channel::<()>();
        let (open, gate) = std::sync::mpsc::channel::<()>();
        let join: JoinHandle<StopReason> = tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _ = entered_tx.send(());
            let _ = gate.recv();
            loop {
                if lane_rx.changed().await.is_err() {
                    return StopReason::Kill;
                }
                if matches!(lane_rx.borrow_and_update().reason, Some(StopReason::Kill)) {
                    status_tx.send_replace(StatusValue::new(ActorStatus::Stopped));
                    return StopReason::Kill;
                }
            }
        });
        let child = Child {
            join,
            lane,
            status,
            started: Arc::new(AtomicUsize::new(0)),
            finished: Arc::new(AtomicUsize::new(0)),
            vetoed: Arc::new(AtomicUsize::new(0)),
        };
        (child, Turn { entered, open })
    }

    /// A child whose task has ended with `reason`, its status plane stopped,
    /// before this returns.
    async fn ended_child(reason: StopReason) -> Child {
        let (lane, _lane_rx) = StopLane::new();
        let (status_tx, status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let join: JoinHandle<StopReason> = tokio::spawn(async move {
            status_tx.send_replace(StatusValue::new(ActorStatus::Stopped));
            reason
        });
        while !join.is_finished() {
            tokio::task::yield_now().await;
        }
        Child {
            join,
            lane,
            status,
            started: Arc::new(AtomicUsize::new(0)),
            finished: Arc::new(AtomicUsize::new(0)),
            vetoed: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn attempt_honors_an_order_written_before_its_factory_runs(_paused: bool) {
        let (ctl, mut io) = slot_pair(Default::default());
        assert!(
            !cancelled_before_factory(&mut io),
            "no order: the factory runs"
        );
        assert!(ctl.watch().cell.borrow().cancelled.not());
        ctl.order_stop(Shutdown::Timeout(Duration::from_millis(50)), false);
        assert!(
            cancelled_before_factory(&mut io),
            "an order cancels the attempt"
        );
        drop(io);
        let mut watch = ctl.watch();
        match bounded(await_cell(&mut watch, 1)).await {
            CellOutcome::Cancelled => {}
            other => panic!("expected the cancel, got {other:?}"),
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
            trap_extra: std::time::Duration::ZERO,
            trap_infinity: false,
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

    async fn attempt_honors_an_order_written_during_init_with_the_window_from_the_order(
        paused: bool,
    ) {
        let rig = attempt_rig(
            spawn_vetoing_child(ONE_TURN, false),
            None,
            DEFAULT_KILL_GRACE,
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let window = Duration::from_millis(200);
        let ordered_at = Instant::now();
        rig.ctl.order_stop(Shutdown::Timeout(window), false);
        let outcome = bounded(rig.run).await.unwrap();
        let took = ordered_at.elapsed();
        match outcome {
            RestartOutcome::Stopped {
                reason,
                incarnation,
                ..
            } => {
                assert!(matches!(reason, StopReason::Kill), "{reason:?}");
                assert_eq!(incarnation, 3);
            }
            _ => panic!("expected the incarnation to end under the order"),
        }
        assert!(took >= window, "the window counts from the order: {took:?}");
        assert!(
            took < upper(paused, window + DEFAULT_KILL_GRACE),
            "{took:?}"
        );
        assert!(
            rig.child.vetoed.load(Ordering::SeqCst) >= 1,
            "the child was asked politely first"
        );
        assert!(matches!(
            rig.ctl.fate().map(|fate| fate.reason),
            Some(StopReason::Kill)
        ));
    }

    async fn attempt_start_timeout_and_order_whichever_ends_first(paused: bool) {
        // The start timeout ends first: its expiry raises the first Kill and
        // the start is reported as timed out.
        let spawned = Instant::now();
        let rig = attempt_rig(
            spawn_vetoing_child(ONE_TURN, false),
            Some(Duration::from_millis(100)),
            DEFAULT_KILL_GRACE,
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        rig.ctl
            .order_stop(Shutdown::Timeout(Duration::from_millis(500)), false);
        match bounded(rig.run).await.unwrap() {
            RestartOutcome::Failed { reason, .. } => assert!(
                matches!(
                    reason,
                    StopReason::Failure(ActorError::Spawn(SpawnError::StartTimeout))
                ),
                "{reason:?}"
            ),
            _ => panic!("expected the start failure"),
        }
        let took = spawned.elapsed();
        assert!(took >= Duration::from_millis(100), "{took:?}");
        assert!(
            took < upper(paused, Duration::from_millis(100) + DEFAULT_KILL_GRACE),
            "{took:?}"
        );

        // The order's window ends first: the incarnation ends under the order.
        let spawned = Instant::now();
        let rig = attempt_rig(
            spawn_vetoing_child(ONE_TURN, false),
            Some(Duration::from_millis(2400)),
            DEFAULT_KILL_GRACE,
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        rig.ctl
            .order_stop(Shutdown::Timeout(Duration::from_millis(50)), false);
        match bounded(rig.run).await.unwrap() {
            RestartOutcome::Stopped { reason, .. } => {
                assert!(matches!(reason, StopReason::Kill), "{reason:?}")
            }
            _ => panic!("expected the stop under the order"),
        }
        let took = spawned.elapsed();
        assert!(took >= Duration::from_millis(70), "{took:?}");
        assert!(
            took < upper(paused, Duration::from_millis(70) + DEFAULT_KILL_GRACE),
            "{took:?}"
        );
    }

    async fn attempt_writes_no_fate_while_its_task_runs() {
        let (child, turn) = stuck_child();
        let rig = attempt_rig(child, None, DEFAULT_KILL_GRACE);
        bounded(turn.entered).await.expect("the turn began");
        rig.ctl
            .order_stop(Shutdown::Timeout(Duration::from_millis(50)), false);
        let mut watch = rig.ctl.watch();
        match bounded(await_cell(&mut watch, 3)).await {
            CellOutcome::Unresponsive => {}
            other => panic!("expected the verdict, got {other:?}"),
        }
        assert!(rig.ctl.fate().is_none(), "no fate while the task runs");
        assert!(
            !rig.run.is_finished(),
            "the attempt keeps waiting for the task"
        );
        assert!(
            timeout(Duration::from_millis(50), rig.verdicts.notified())
                .await
                .is_ok(),
            "the verdict wakes the supervisor"
        );
        // Once the turn ends the attempt records the fate and reports.
        turn.open.send(()).expect("the turn waits at its gate");
        match bounded(rig.run).await.unwrap() {
            RestartOutcome::Stopped { reason, .. } => {
                assert!(matches!(reason, StopReason::Kill), "{reason:?}")
            }
            _ => panic!("expected the stop under the order"),
        }
        assert!(rig.ctl.fate().is_some());
        assert!(
            rig.ctl.is_unresponsive(),
            "the verdict stays beside the fate"
        );
    }

    async fn ladder_writes_the_verdict_and_signals_once() {
        let verdicts: Arc<Verdicts> = Arc::default();
        let (child, turn) = stuck_child();
        let (ctl, io) = slot_pair(verdicts.clone());
        io.publish(
            child.lane.clone(),
            child.status.clone(),
            OwnStop::worker(DEFAULT_KILL_GRACE),
            None,
        );
        let mut rig = rig_over(child, ctl, io, DEFAULT_KILL_GRACE);
        bounded(turn.entered).await.expect("the turn began");
        rig.ctl
            .order_stop(Shutdown::Timeout(Duration::from_millis(50)), true);
        let mut watch = rig.ctl.watch();
        match bounded(await_cell(&mut watch, 1)).await {
            CellOutcome::Unresponsive => {}
            other => panic!("expected the verdict, got {other:?}"),
        }
        assert!(
            timeout(Duration::from_millis(50), verdicts.notified())
                .await
                .is_ok(),
            "the verdict signals the supervisor"
        );
        assert!(
            timeout(Duration::from_millis(300), verdicts.notified())
                .await
                .is_err(),
            "and signals it once"
        );
        turn.open.send(()).expect("the turn waits at its gate");
        let _ = bounded(rig.death_set.join_next()).await;
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

    async fn lost_runner_of_a_stuck_incarnation_gets_the_verdict_after_its_abort() {
        let (child, turn) = stuck_child();
        bounded(turn.entered).await.expect("the turn began");
        let verdicts: Arc<Verdicts> = Arc::default();
        let (ctl, io) = slot_pair(verdicts.clone());
        io.publish(
            child.lane.clone(),
            child.status.clone(),
            OwnStop::worker(DEFAULT_KILL_GRACE),
            None,
        );
        let mut sup = state_with_a_child("lost-stuck", ctl, 5);
        sup.verdicts = verdicts.clone();
        // The guard feeds a running reaper: the verdict follows the abort it
        // issues once the grace has passed.
        let system = dummy_system();
        let reaper_tx = system.reaper_handle(&tokio::runtime::Handle::current());
        let guard = ChildLinkGuard::new(
            child.lane.clone(),
            child.join.abort_handle(),
            reaper_tx,
            crate::actor::supervision::rules::InheritedStop::Kill {
                grace: DEFAULT_KILL_GRACE,
            },
            system.clone(),
            tokio::runtime::Handle::current(),
        );
        // The stand-in for the runner holds the slot and the guard, and
        // fails; its guard raises `Kill` on the incarnation's lane.
        let task = sup.death_set.spawn(async move {
            let _io = io;
            let _guard = guard;
            let _join = child.join;
            tokio::task::yield_now().await;
            if std::hint::black_box(true) {
                panic!("runner failed");
            }
            (ActorId::from("lost-stuck"), StopReason::Graceful, 5)
        });
        sup.task_owner.insert(
            task.id(),
            (ActorId::from("lost-stuck"), 5, TaskPlane::Death),
        );
        let err = bounded(sup.death_set.join_next_with_id())
            .await
            .unwrap()
            .expect_err("the runner failed");
        let called = Instant::now();
        let _ = sup.lost_death_task(err);
        let is_flagged = |sup: &SupervisionState| {
            sup.registry
                .get(&ActorId::from("lost-stuck"))
                .is_some_and(|child| child.ctl.is_unresponsive())
        };
        // No report and no verdict while the incarnation is inside its grace.
        // The observation counts only if the grace and the settle time had
        // not passed on the test's own clock by the time it was made, so the
        // clock is read after it.
        tokio::time::sleep(DEFAULT_KILL_GRACE).await;
        let flagged_in_the_grace = is_flagged(&sup);
        if called.elapsed() < DEFAULT_KILL_GRACE + ABORT_SETTLE {
            assert!(!flagged_in_the_grace);
        }
        // The grace and the settle time passed: the verdict is recorded and
        // the supervisor woken, once.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !is_flagged(&sup) {
            assert!(Instant::now() < deadline, "the verdict flag is never set");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(called.elapsed() >= DEFAULT_KILL_GRACE + ABORT_SETTLE);
        assert!(
            timeout(Duration::from_millis(50), verdicts.notified())
                .await
                .is_ok(),
            "the supervisor was woken"
        );
        assert!(
            timeout(Duration::from_millis(100), verdicts.notified())
                .await
                .is_err(),
            "once"
        );
        // After the turn the incarnation reads the guard's `Kill`: exactly
        // one report, with `Kill`.
        turn.open.send(()).expect("the turn waits at its gate");
        let (id, reason, incarnation) = bounded(sup.death_set.join_next()).await.unwrap().unwrap();
        assert_eq!((id.as_str(), incarnation), ("lost-stuck", 5));
        assert!(matches!(reason, StopReason::Kill), "{reason:?}");
        assert!(
            sup.death_set.join_next().await.is_none(),
            "exactly one report"
        );
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
                trap_extra: std::time::Duration::ZERO,
                trap_infinity: false,
            };
            let occ = occupant(kind, Some(link));
            let predecessor = Predecessor::untracked(root.watch());
            drop(root);
            (predecessor, occ)
        }

        // - Writers ---------------------------------------------------------------

        // - The control -----------------------------------------------------------

        /// A predecessor owed an abort past its subtree's deadline obeys its
        /// late `Kill` and ends on its own, with a worker below it whose
        /// runner its end cancels: the worker's guard owes the worker's abort
        /// only once the runtime drops that runner. The subtree's verdict
        /// waits for that cascade: the end of an owed actor counts as its
        /// abort, so no verdict comes inside the settle time after the end,
        /// in which the worker's abort is owed and issued. The rounds run on
        /// real time, since the order the runtime polls the woken wait and
        /// the cancelled runner in is what the guarantee covers; a host that
        /// stalls the process for longer than the settle time inside that
        /// cascade can still see the verdict first, which is the settle
        /// time's own residual, so a round counts a verdict as wrong only
        /// when it comes inside the settle time after the end.
        ///
        /// A host that stalls the process past the wait's cap before the
        /// predecessor ends has the wait give its verdict on its own bound
        /// first, which the guarantee does not cover. A round whose end
        /// came that late is not counted, and the test runs rounds until
        /// enough of them ended in time.
        mod ends_while_owed {
            use super::*;

            /// The worker's link guard, with a zero kill grace: its drop
            /// owes the worker's abort and issues it at once.
            struct OweOnDrop(AbortRecord, tokio::task::AbortHandle);

            impl Drop for OweOnDrop {
                fn drop(&mut self) {
                    self.0.owe();
                    self.0.abort(&self.1);
                }
            }

            /// The worker's own budget: the wait's bound, and its cap past
            /// the deadline, both long enough for a stalled host to leave the
            /// wait held when the predecessor ends.
            const WORKER_OWN: Duration = Duration::from_millis(300);

            /// Rounds whose end came in time that the test needs, and the
            /// most rounds it tries to get them.
            const VALID_ROUNDS: usize = 4;
            const MOST_ROUNDS: usize = 200;

            async fn body() {
                let mut valid = 0;
                let mut attempts = 0;
                let mut wrong = 0;
                while valid < VALID_ROUNDS && attempts < MOST_ROUNDS {
                    attempts += 1;
                    let root = SubtreeHold::new(None);
                    let p_rec = AbortRecord::new();
                    let p = SubtreeHold::with_record(
                        Some(SubtreeLink {
                            parent: root.token(),
                            own: Duration::ZERO,
                            infinity: false,
                            trap_extra: std::time::Duration::ZERO,
                            trap_infinity: false,
                        }),
                        p_rec.clone(),
                    );
                    let g_rec = AbortRecord::new();
                    let g = SubtreeHold::with_record(
                        Some(SubtreeLink {
                            parent: p.token(),
                            own: WORKER_OWN,
                            infinity: false,
                            trap_extra: std::time::Duration::ZERO,
                            trap_infinity: false,
                        }),
                        g_rec.clone(),
                    );
                    // The worker: its actor holds its token; its runner holds
                    // the guard that owes its abort when the runner is dropped.
                    let g_actor = tokio::spawn(async move {
                        let _g = g;
                        std::future::pending::<()>().await;
                    });
                    let guard = OweOnDrop(g_rec.clone(), g_actor.abort_handle());
                    let g_runner = tokio::spawn(async move {
                        let _guard = guard;
                        std::future::pending::<()>().await;
                    });
                    // The predecessor is owed its abort (an entry whose stage
                    // the reaper reaches late).
                    p_rec.owe();
                    let bound = p.watch().release_bound().unwrap();
                    let predecessor = Predecessor::untracked(p.watch());
                    // The wait arms its deadline at `bound` from a moment no
                    // earlier than this one.
                    let armed_no_earlier = Instant::now();
                    let waiter = tokio::spawn(async move {
                        let action = await_subtree(&predecessor).await;
                        (action, Instant::now())
                    });
                    tokio::time::sleep(bound + Duration::from_millis(30)).await;
                    // The predecessor obeys its late `Kill` and ends: its end
                    // cancels its children's runners, then its hold drops.
                    let (go_tx, go_rx) = oneshot::channel::<()>();
                    let runner_abort = g_runner.abort_handle();
                    let p_task = tokio::spawn(async move {
                        let _ = go_rx.await;
                        runner_abort.abort();
                        let ended = Instant::now();
                        drop(p);
                        ended
                    });
                    tokio::task::yield_now().await;
                    go_tx.send(()).unwrap();
                    let (action, verdict_at) = bounded(waiter).await.unwrap();
                    let ended = p_task.await.unwrap();
                    let _ = g_runner.await;
                    // The wait holds for an owed abort until its cap, which
                    // is `bound` past its deadline. An end that came later
                    // is a stalled host's, and the wait's own bound decided.
                    let in_time = ended < armed_no_earlier + bound + bound;
                    if in_time {
                        valid += 1;
                        if action == WaitAction::Escalate {
                            assert!(g_rec.view().0.is_some(), "the worker's abort followed");
                            if verdict_at < ended + ABORT_SETTLE {
                                wrong += 1;
                            }
                        } else {
                            assert_eq!(action, WaitAction::Proceed);
                        }
                    }
                    g_actor.abort();
                    let _ = g_actor.await;
                    drop(root);
                }
                assert!(
                    valid > 0,
                    "no predecessor ended in time in {attempts} rounds"
                );
                assert_eq!(
                    wrong, 0,
                    "{wrong} of {valid} verdicts came inside the settle time after the end"
                );
            }
        }

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
                    trap_extra: std::time::Duration::ZERO,
                    trap_infinity: false,
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

        /// The runner's ladder owes a `Kill`, the task obeys and ends, a
        /// forwarder keeps the predecessor's hold for `drop_after` past the
        /// end, and the successor's wait starts at the end. Returns the
        /// wait's answer and how long after the end it came.
        async fn forwarder_outlives_the_end(drop_after: Duration) -> (WaitAction, Duration) {
            let (lane, lane_rx) = StopLane::new();
            let pred = SubtreeHold::with_record(None, lane.abort_record().clone());
            let forwarder_hold = pred.clone();
            let task_hold = pred.clone();
            let predecessor = Predecessor::untracked(pred.watch());
            drop(pred);
            let (status_tx, _status) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
            let mut join = tokio::spawn({
                let task = obeying(lane_rx, status_tx, false);
                async move {
                    let _hold = task_hold;
                    task.await.unwrap()
                }
            });
            let (ctl, mut io) = slot_pair(Default::default());
            ctl.write_order(Shutdown::Kill, false);
            let ladder = tokio::spawn({
                let lane = lane.clone();
                async move { run_ladder(&mut join, None, &lane, &mut io, GRACE).await }
            });
            let reason = bounded(ladder).await.unwrap();
            assert!(matches!(reason, StopReason::Kill), "{reason:?}");
            let ended = Instant::now();
            let dropper = tokio::spawn(async move {
                tokio::time::sleep(drop_after).await;
                drop(forwarder_hold);
            });
            let action = bounded(await_subtree(&predecessor)).await;
            let waited = Instant::now() - ended;
            dropper.abort();
            drop(ctl);
            (action, waited)
        }
    }

    /// A wait on the cell owns the verdict while it is open or once it told
    /// its caller. A wait dropped without telling anyone hands the verdict
    /// back: the loop is woken to look for a caller.
    #[tokio::test]
    async fn a_wait_on_the_cell_owns_the_verdict_until_it_is_dropped_or_told() {
        let verdicts = Arc::new(Verdicts::default());
        let (ctl, io) = slot_pair(verdicts.clone());
        let woken = |verdicts: &Arc<Verdicts>| {
            let verdicts = verdicts.clone();
            async move { timeout(Duration::ZERO, verdicts.notified()).await.is_ok() }
        };
        assert!(!ctl.verdict_has_audience());
        let wait = ctl.watch();
        assert!(ctl.verdict_has_audience(), "an open wait owns the verdict");
        io.write_verdict();
        assert!(woken(&verdicts).await, "the verdict woke the loop");
        drop(wait);
        assert!(!ctl.verdict_has_audience(), "a dropped wait owns nothing");
        assert!(
            woken(&verdicts).await,
            "a wait dropped with the verdict untold woke the loop again"
        );

        let told = ctl.watch();
        told.mark_reported();
        drop(told);
        assert!(ctl.verdict_has_audience(), "a told verdict stays owned");
        assert!(
            !woken(&verdicts).await,
            "a told verdict leaves the loop asleep"
        );
    }

    // - attempts read from their incarnation's status plane ----------------

    struct Idle;

    impl crate::Actor for Idle {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::ActorContext<Self>,
        ) -> crate::ActorResult<()> {
            Ok(())
        }
    }

    /// A slot whose attempt published an incarnation, with the incarnation's
    /// lane and status plane on one record, the plane reading `status`.
    fn published_slot(status: ActorStatus) -> (SlotCtl, SlotIo, StatusTx, StopLane) {
        let (ctl, io) = slot_pair(Default::default());
        let (lane, _lane_rx) = StopLane::new();
        let (status_tx, status_rx) = lane.abort_record().status_channel();
        status_tx.send_if_modified(|value| {
            value.status = status;
            true
        });
        io.publish(
            lane.clone(),
            status_rx,
            OwnStop::worker(DEFAULT_KILL_GRACE),
            None,
        );
        (ctl, io, status_tx, lane)
    }

    /// An attempt task lost after its incarnation was spawned is followed to
    /// the incarnation's end by a death task, which the supervisor claims
    /// before the attempt settles: an incarnation whose final reason is
    /// already written, its end not yet recorded by its registry guard, is
    /// owed when the settle lowers the count of attempts in flight.
    async fn a_lost_attempt_task_claims_its_incarnation_before_it_settles() {
        let sys = ActorSystem::create_with(
            format!("slot-claim-{}", uuid::Uuid::new_v4()),
            crate::system::SystemConfig::default().with_turn_tracking(),
        )
        .unwrap();
        let boss = crate::actor::ActorExt::spawn(Idle)
            .on_system(&sys)
            .await
            .unwrap();
        let record = boss.stop_lane_ref().abort_record().clone();
        let identity =
            crate::telemetry::identity_for(sys.telemetry_mode(), &record, Some(&*sys), None)
                .expect("the supervisor has an identity");

        let (ctl, io, status_tx, _lane) = published_slot(ActorStatus::Initializing);
        assert!(StatusValue::write_reason(
            &status_tx,
            StopReason::Failure(ActorError::user("the start is refused")),
            false
        ));
        // An id of its own: the end hooks are keyed by id across the process.
        let id = format!("claimed-attempt-{}", uuid::Uuid::new_v4());
        let child = ActorId::from(id.as_str());
        let mut sup = state_with_a_child(&id, ctl, 7);
        sup.registry.adopt_owner(&record);
        if let Some(entry) = sup.registry.get_mut(&child) {
            entry.lifecycle = ChildLifecycle::Restarting {
                incarnation: 7,
                next: 8,
            };
        }
        let ticket = AttemptTicket::begin(&record, Some(&*sys), boss.id()).expect("a ticket");
        sup.note_attempt(&child, 8, ticket);
        let task = sup.restart_set.spawn(async move {
            let _io = io;
            tokio::task::yield_now().await;
            if std::hint::black_box(true) {
                panic!("attempt task failed");
            }
            RestartOutcome::Cancelled {
                child_id: ActorId::from("lost"),
                incarnation: 8,
                drop_panic: None,
            }
        });
        sup.task_owner
            .insert(task.id(), (child.clone(), 8, TaskPlane::Restart));
        let Err(err) = bounded(sup.restart_set.join_next_with_id()).await.unwrap() else {
            panic!("the attempt task failed");
        };

        let seen: Arc<std::sync::Mutex<Option<(u32, u32)>>> = Arc::default();
        {
            let (seen, identity) = (seen.clone(), identity.clone());
            crate::actor::runtime::end_hooks::attach(
                crate::actor::runtime::end_hooks::SETTLED,
                child.clone(),
                move || {
                    *seen.lock().unwrap() = Some((
                        identity.restarts_in_flight(),
                        identity.owed_deaths_for_test(),
                    ));
                },
            );
        }
        let (outcome, _, _) = sup.lost_restart_task(err);
        assert!(
            outcome.is_none(),
            "the incarnation is followed, not reported"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            Some((0, 1)),
            "(attempts in flight, deaths owed) when the attempt settled"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lost_attempt_task_claims_its_incarnation_before_it_settles_multi_thread() {
        a_lost_attempt_task_claims_its_incarnation_before_it_settles().await;
    }

    /// The final collection's read of an attempt still in its plane, by what
    /// the incarnation's status plane and the slot hold. An end with its
    /// reason written is read as the attempt reports itself: a failed start
    /// as a failure, as a stop when the supervisor ordered one, and as the
    /// start timeout when the attempt's start timeout cut it; a completed
    /// start is handed over unless an order or the start timeout stands, the
    /// timeout giving the start timeout when it raised the cut and the
    /// failure when a `Kill` already stood. An incarnation that has not ended
    /// is left to the teardown, and an end already published is awaited.
    #[test]
    fn the_final_collection_reads_an_attempt_as_the_attempt_reports_itself() {
        #[derive(Debug, PartialEq)]
        enum Want {
            Failed,
            Stopped,
            StartTimeout,
            Adopts,
            Leave,
            Await,
        }
        #[derive(Clone, Copy)]
        enum Timeout {
            None,
            Fired,
            Cut,
        }
        let refused = || StopReason::Failure(ActorError::user("the start is refused"));
        let rows = [
            (
                "start failed",
                ActorStatus::Initializing,
                true,
                false,
                Timeout::None,
                Want::Failed,
            ),
            (
                "start failed, ordered",
                ActorStatus::Initializing,
                true,
                true,
                Timeout::None,
                Want::Stopped,
            ),
            (
                "start cut by the timeout",
                ActorStatus::Initializing,
                true,
                false,
                Timeout::Cut,
                Want::StartTimeout,
            ),
            (
                "start completed",
                ActorStatus::Stopping,
                true,
                false,
                Timeout::None,
                Want::Adopts,
            ),
            (
                "start completed, ordered",
                ActorStatus::Stopping,
                true,
                true,
                Timeout::None,
                Want::Stopped,
            ),
            (
                "start completed, the timeout fired under a Kill",
                ActorStatus::Running,
                true,
                false,
                Timeout::Fired,
                Want::Failed,
            ),
            (
                "start completed, cut by the timeout",
                ActorStatus::Running,
                true,
                false,
                Timeout::Cut,
                Want::StartTimeout,
            ),
            (
                "running",
                ActorStatus::Running,
                false,
                false,
                Timeout::None,
                Want::Leave,
            ),
            (
                "end published",
                ActorStatus::Stopped,
                true,
                false,
                Timeout::None,
                Want::Await,
            ),
        ];
        for (case, status, reason, ordered, timeout, want) in rows {
            let (ctl, io, status_tx, _lane) = published_slot(status);
            if reason {
                assert!(StatusValue::write_reason(&status_tx, refused(), false));
            }
            if ordered {
                ctl.write_order(Shutdown::default(), false);
            }
            match timeout {
                Timeout::None => {}
                Timeout::Fired => io.mark_start_timeout(false),
                Timeout::Cut => io.mark_start_timeout(true),
            }
            let child = ActorId::from("read");
            let got = match final_attempt(&ctl, &child, 3) {
                FinalAttempt::Await => Want::Await,
                FinalAttempt::Adopts => Want::Adopts,
                FinalAttempt::Leave => Want::Leave,
                FinalAttempt::Report(outcome) => match *outcome {
                    RestartOutcome::Stopped {
                        incarnation: 3,
                        reason,
                        ..
                    } => {
                        assert_eq!(reason, refused(), "{case}");
                        Want::Stopped
                    }
                    RestartOutcome::Failed {
                        incarnation: 3,
                        reason: StopReason::Failure(ActorError::Spawn(SpawnError::StartTimeout)),
                        ..
                    } => Want::StartTimeout,
                    RestartOutcome::Failed {
                        incarnation: 3,
                        reason,
                        ..
                    } => {
                        assert_eq!(reason, refused(), "{case}");
                        Want::Failed
                    }
                    _ => panic!("{case}: another outcome"),
                },
            };
            assert_eq!(got, want, "{case}");
        }
    }
}
