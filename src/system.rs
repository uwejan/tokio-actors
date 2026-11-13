//! Actor system providing named actor registration and coordinated shutdown.
//!
//! The actor registry and coordinated shutdown; the public types are also
//! re-exported at the crate root.

use std::any::{Any, TypeId};
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use dashmap::DashMap;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant;

use crate::actor::handle::{emit_stop_requested, ActorHandle, SubtreeObserver};
use crate::actor::panic::{contain_waker_panic, send_contained};
use crate::actor::runtime::{bounded_wait, saturating_deadline};
use crate::actor::subtree::{AbortRecord, SubtreeHold};
use crate::actor::supervision::{rules, ABORT_SETTLE};
use crate::actor::Actor;
use crate::error::{SendError, SpawnError};
use crate::helper::HelperKind;
use crate::telemetry::{
    Delivery, EventKind, KillStepKind, MonitorConfig, MonitorError, MonitorSettings, TelemetryEvent,
};
use crate::types::{
    ActorId, ActorStatus, Incarnation, RaiseResult, ShutdownReport, StatusRx, StatusTx, StopLane,
    StopOutcome, StopReason, StopReport, TurnClock,
};

// ---------------------------------------------------------------------------
// Global systems registry
// ---------------------------------------------------------------------------

/// The name of the ambient system that bare spawns join. Its slot in the
/// process map is always strong.
const DEFAULT_SYSTEM: &str = "default";

static SYSTEMS: OnceLock<DashMap<String, Slot>> = OnceLock::new();

fn systems() -> &'static DashMap<String, Slot> {
    SYSTEMS.get_or_init(DashMap::new)
}

/// The source of every system's generation. Generations start at 1: `0`
/// stands for "no system" in a handle no spawn made.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// What the process map holds for one name.
///
/// The slot holds its system strongly from the system's creation until the
/// shutdown that completes it (the one that stores `Defunct`), and weakly from
/// then until [`ActorSystem::reactivate`] puts the strong reference back. The
/// ambient `"default"` system is never demoted. A weakly held system leaves
/// the map when its last holder lets go (see `Drop for ActorSystem`).
///
/// No `Arc` is created while a shard guard of the map is held unless it
/// escapes by return, and none is dropped under one: a system's drop removes
/// its own slot, which takes the same shard lock. Liveness under a guard is
/// read from the counts (`is_live`), and a `Weak` is upgraded only after the
/// guard has ended.
struct Slot {
    /// The generation of the system that owns this slot.
    generation: u64,
    weak: Weak<ActorSystem>,
    strong: Option<Arc<ActorSystem>>,
}

impl Slot {
    fn new(system: &Arc<ActorSystem>) -> Self {
        Self {
            generation: system.generation,
            weak: Arc::downgrade(system),
            strong: Some(system.clone()),
        }
    }

    /// Whether the system can still be reached. A count of zero is final; a
    /// stale positive count only makes a caller see a system that has just
    /// died, which is a legal order.
    fn is_live(&self) -> bool {
        self.strong.is_some() || self.weak.strong_count() > 0
    }
}

// ---------------------------------------------------------------------------
// ShutdownPolicy
// ---------------------------------------------------------------------------

/// Policy controlling how [`ActorSystem::shutdown`] behaves.
#[derive(Debug, Clone)]
pub struct ShutdownPolicy {
    /// The shutdown's global deadline, counted from the call's first poll.
    /// After it, all remaining actors are force-stopped: a shutdown can run on
    /// for the longest kill grace among the roots it forces plus the abort
    /// settle time, then the longest kill grace among the leftovers it forces
    /// plus the abort settle time, and, for either of those two steps, that
    /// time once more plus the abort settle time when the abort of an actor it
    /// forces, scheduled with the system, is issued late. The system holds one
    /// sweep deadline, the earliest of the deadlines of the sweeps that have
    /// started, each taken as no earlier than the moment its sweep begins, and
    /// clears it when the last shutdown in flight ends, provided a shutdown has
    /// completed since the system was last `Active`. Until then, a held
    /// deadline that is earlier than this call's own bounds this call's
    /// leftovers too, whether the sweep that set it is still running, was cut
    /// short or has completed. A timeout beyond 30 years is treated as 30
    /// years.
    ///
    /// Default: 30 seconds.
    pub timeout: Duration,
    /// Maximum time to wait for each root actor to stop after its stop request:
    /// it covers the root's `pre_stop`, which may veto, and for a root
    /// supervisor its whole teardown (its own `on_stopped`, the final
    /// deliveries of its pending child deaths and the stops of all of its
    /// children), and each wait is capped by what is left of the global
    /// deadline. If a root doesn't stop within this period, it receives
    /// `StopReason::Kill`. A period beyond 30 years is treated as 30 years. A
    /// root supervisor's window covers its children only when it is at least
    /// the sum of the children's stop budgets (each child's `Shutdown`, plus
    /// its kill grace, plus the short wait that confirms an abort), or the
    /// largest of them for a `SimpleOneForOne` pool. A shorter window escalates
    /// the root to `Kill`, and its children still running then take the path of
    /// a killed supervisor's children: workers that do not trap exit signals
    /// receive `Kill`, and a child that is itself a supervisor, or a worker
    /// that traps, stops in order within its own `Shutdown`. It
    /// bounds only the roots' request step: a non-root actor still registered
    /// once every root has been handled is waited on, bounded by the global
    /// deadline of [`timeout`](Self::timeout) (or the earlier deadline the
    /// system holds for a sweep already started), and force-stopped if it is
    /// still running then (see [`timeout`](Self::timeout) for how long a forced
    /// stop can run on after the deadline).
    ///
    /// Default: 5 seconds.
    pub per_actor_timeout: Duration,
}

impl Default for ShutdownPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            per_actor_timeout: Duration::from_secs(5),
        }
    }
}

/// Configuration for creating a new [`ActorSystem`].
#[derive(Debug, Clone, Default)]
pub struct SystemConfig {
    /// Shutdown policy for this system.
    pub shutdown_policy: ShutdownPolicy,
    /// Initial stuck-turn watchdog threshold for this system. `None` (the
    /// default) leaves the watchdog off: the watchdog times turns only while a
    /// threshold is set, so with this left `None`,
    /// [`ActorSystem::turn_elapsed`] returns `None` for every actor and
    /// [`ActorSystem::stuck_actors`] is empty. Setting a threshold adds one
    /// monotonic clock read per callback of every actor in the
    /// system. A turn already in flight when the threshold is first applied
    /// is not timed; the next turn that actor starts is. See
    /// [`ActorSystem::set_turn_warning_threshold`] for what this controls.
    pub turn_warning_threshold: Option<Duration>,
    /// Whether turn tracking starts on (the default is off). See
    /// [`ActorSystem::set_turn_tracking`].
    pub turn_tracking: bool,
    /// Whether wait-cycle detection starts on (the default is off). See
    /// [`ActorSystem::set_wait_cycle_detection`].
    pub wait_cycle_detection: bool,
}

impl SystemConfig {
    /// Sets the initial stuck-turn watchdog threshold (see
    /// [`ActorSystem::set_turn_warning_threshold`]).
    pub fn with_turn_warning_threshold(mut self, threshold: Duration) -> Self {
        self.turn_warning_threshold = Some(threshold);
        self
    }

    /// Starts the system with turn tracking on (see
    /// [`ActorSystem::set_turn_tracking`]).
    pub fn with_turn_tracking(mut self) -> Self {
        self.turn_tracking = true;
        self
    }

    /// Starts the system with wait-cycle detection on (see
    /// [`ActorSystem::set_wait_cycle_detection`]).
    pub fn with_wait_cycle_detection(mut self) -> Self {
        self.wait_cycle_detection = true;
        self
    }
}

// ---------------------------------------------------------------------------
// Type-erased actor handle
// ---------------------------------------------------------------------------

struct AnyActorHandle {
    type_id: TypeId,
    handle: Box<dyn StoredHandle>,
    /// Clone of the actor's stop lane. Enough on its own to signal a stop or
    /// kill without downcasting, and cheap to clone out of a `DashMap` `Ref`
    /// before an `.await` (raising it is synchronous and never waits for capacity anyway).
    stop_lane: StopLane,
    /// Runtime status plane, threaded straight through from the
    /// `ActorHandle`: the cell is created before the actor's task
    /// is spawned, so this is always available at registration time (unlike
    /// `abort`, below).
    status_rx: StatusRx,
    /// This actor's turn clock, handed in at registration time (the clock
    /// lives with the run loop and this registry entry, never with
    /// `ActorHandle`): backs [`ActorSystem::turn_elapsed`] and
    /// [`ActorSystem::stuck_actors`] without downcasting.
    turn_clock: TurnClock,
    /// The task's abort handle. `None` for the brief window between
    /// registration (which happens before the task is spawned, so the
    /// name-claim is visible pre-spawn) and the post-spawn `attach_abort`
    /// call; treated as not-yet-abortable by a shutdown sweep that lands in
    /// that window.
    abort: Option<AbortHandle>,
    /// `None` for actors registered via the top-level `SpawnBuilder` path;
    /// for supervised children (`spawn_child` and its restart path) the id
    /// and incarnation number of the supervisor that spawned them, by which
    /// the child's telemetry identity finds its supervisor's. System shutdown
    /// walks the roots; each root's own supervision cascade takes its subtree
    /// down in turn.
    parent: Option<(ActorId, u64)>,
    /// The registered run's incarnation number, drawn once from the process
    /// counter when the run's block is made (deliberately not a wall-clock
    /// timestamp): shutdown stops roots in reverse order of it, and a counter
    /// is race-free where two spawns landing in the same clock tick would not
    /// be. Two spawns that overlap in time are ordered by the order their
    /// blocks were made.
    registration_seq: u64,
    /// The time this actor is given, after a `Kill` is raised on it, to
    /// reach its next turn boundary before its task is aborted. Every kill
    /// path that finds the actor through the registry reads it here.
    kill_grace: Duration,
    /// The runtime that drives this actor's task. An abort takes effect only
    /// when that runtime next polls the task, so the kill backstop is
    /// scheduled on this runtime's reaper and on no other.
    runtime: Handle,
    /// The earliest deadline any `Kill` raised on this incarnation asked for,
    /// in nanoseconds since the system's epoch; `0` when none did. Every
    /// raiser records its deadline here before it raises, so an attach that
    /// finds the `Kill` already raised arms the abort for the earliest of
    /// them. See [`raise_kill_declared`].
    kill_due: u64,
}

/// What a snapshot copied out of one registry entry.
struct Seen {
    id: ActorId,
    seq: u64,
    /// The address of the incarnation's abort record.
    key: usize,
    /// The address of the incarnation's subtree token, and of its
    /// supervisor's.
    token: Option<usize>,
    parent: Option<usize>,
    /// The identity of the incarnation's turn clock, which is the caller token
    /// of the wait graph.
    clock: u64,
    /// Present while the incarnation's end is not final; an entry without it
    /// is kept only so its live children can name it as their supervisor.
    live: Option<LiveRead>,
}

/// What a snapshot reads of an actor that has not ended.
struct LiveRead {
    status: ActorStatus,
    mail: MailView,
    children: usize,
    in_turn: Option<Duration>,
    suspended: bool,
    coupling: crate::telemetry::CouplingInfo,
}

/// The reference of the entry `at`, with its ancestry, built from the entries
/// the snapshot copied out: each supervisor is found by its subtree token, so
/// the incarnation named is the one that spawned the child, never one looked
/// up by id. A supervisor the registry does not hold ends the chain there.
fn ref_of(
    seen: &[Seen],
    by_token: &HashMap<usize, usize>,
    refs: &mut [Option<crate::telemetry::ActorRef>],
    at: usize,
) -> crate::telemetry::ActorRef {
    use crate::telemetry::ActorRef;

    let mut chain = vec![at];
    let mut top = at;
    while refs[top].is_none() {
        let next = seen[top]
            .parent
            .and_then(|parent| by_token.get(&parent).copied())
            .filter(|next| !chain.contains(next));
        match next {
            Some(next) => {
                chain.push(next);
                top = next;
            }
            None => break,
        }
    }
    let mut above = if refs[top].is_some() {
        chain.pop();
        refs[top].clone()
    } else {
        None
    };
    while let Some(index) = chain.pop() {
        let row = &seen[index];
        let built = match &above {
            Some(parent) => ActorRef::new_child(row.id.clone(), row.seq, parent),
            None => ActorRef::new_root(row.id.clone(), row.seq),
        };
        refs[index] = Some(built.clone());
        above = Some(built);
    }
    refs[at]
        .clone()
        .expect("the chain ends at the entry asked for")
}

/// One read of the state quiescence compares.
#[derive(PartialEq, Eq)]
struct QuietPass {
    /// Registration number, turns begun, inside a turn, mailbox length and
    /// priority length of each actor whose end is not final.
    rows: Vec<(u64, u64, bool, usize, usize)>,
    /// The restart attempts of all supervisors that have begun and not
    /// settled, and the child deaths their runners landed and the
    /// supervisors have not finished handling.
    in_flight: u64,
    /// Every actor read idle with empty lanes and none in flight.
    idle: bool,
}

/// What a registry entry tells about its incarnation: id, incarnation number,
/// root flag, turn clock id and actor type name.
type EntryFacts = (ActorId, u64, Option<(ActorId, u64)>, u64, &'static str);

/// The numbers a snapshot reads from an actor's handle: its type name and the
/// fill of its two lanes. Each is one or two atomic reads; nothing waits and
/// no actor is asked.
#[derive(Clone, Copy, Debug)]
struct MailView {
    type_name: &'static str,
    mailbox_len: usize,
    mailbox_capacity: usize,
    priority_len: usize,
    priority_capacity: usize,
}

/// What the registry keeps of an actor's handle: the handle itself, for a
/// typed lookup, and the view of its mailbox, which only code that knows the
/// actor type can build. The view is a method of the stored object's own
/// table, so an entry holds no pointer for it.
trait StoredHandle: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn mail_view(&self) -> MailView;
}

impl<A: Actor> StoredHandle for ActorHandle<A> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn mail_view(&self) -> MailView {
        MailView {
            type_name: std::any::type_name::<A>(),
            mailbox_len: self.mailbox_len(),
            mailbox_capacity: self.mailbox_capacity(),
            priority_len: self.priority_len(),
            priority_capacity: self.priority_capacity(),
        }
    }
}

impl AnyActorHandle {
    fn new<A: Actor>(
        handle: &ActorHandle<A>,
        parent: Option<(ActorId, u64)>,
        registration_seq: u64,
        turn_clock: TurnClock,
        kill_grace: Duration,
        runtime: Handle,
    ) -> Self {
        Self {
            type_id: TypeId::of::<ActorHandle<A>>(),
            handle: Box::new(handle.clone()),
            stop_lane: handle.stop_lane(),
            status_rx: handle.status_rx(),
            turn_clock,
            abort: None,
            parent,
            registration_seq,
            kill_grace,
            runtime,
            kill_due: 0,
        }
    }

    /// Keeps the earlier of the recorded deadline and `window` (or the kill
    /// grace) after `now`.
    fn declare_kill(&mut self, now: u64, window: Option<Duration>) {
        let after = window.unwrap_or(self.kill_grace);
        let due = now.saturating_add(u64::try_from(after.as_nanos()).unwrap_or(u64::MAX));
        if self.kill_due == 0 || due < self.kill_due {
            self.kill_due = due;
        }
    }

    fn downcast<A: Actor>(&self) -> Option<ActorHandle<A>> {
        if self.type_id == TypeId::of::<ActorHandle<A>>() {
            self.handle
                .as_any()
                .downcast_ref::<ActorHandle<A>>()
                .cloned()
        } else {
            None
        }
    }
}

/// Queues the `KillStep` event `step` of the incarnation whose abort record is
/// `record`, in the system in hand or the one the record names. It builds
/// nothing when no sink takes the event or the incarnation has no recorded
/// identity, never waits and runs no user code, so the reaper, a drop and the
/// raiser call it with no care for what they hold. `after` is read only when
/// the event is built.
#[inline]
fn push_kill_step(
    record: &AbortRecord,
    system: Option<&ActorSystem>,
    step: KillStepKind,
    after: impl FnOnce() -> Option<Duration>,
) {
    crate::telemetry::push_about(record, system, EventKind::KillStep, |actor| {
        TelemetryEvent::KillStep {
            actor: actor.clone(),
            step,
            after: after(),
        }
    });
}

/// Builds the `AbortFired` step of the incarnation behind `record`, to be
/// queued by [`AbortRecord::abort`] under the record's lock for the abort that
/// writes the record's first instant, so the step is queued once per
/// incarnation whichever abort (the reaper's, a caller's force path, a
/// ladder's, a start bound's) finds the task unfinished first. Takes no lock
/// of the record, queues nothing, and builds nothing unless a sink takes the
/// kind.
pub(crate) fn stage_abort_fired(record: &AbortRecord) -> Option<crate::telemetry::Staged> {
    crate::telemetry::stage_about(record, None, EventKind::KillStep, |actor| {
        TelemetryEvent::KillStep {
            actor: actor.clone(),
            step: KillStepKind::AbortFired,
            after: None,
        }
    })
}

/// The time from now to `deadline`.
fn step_deadline_in(deadline: Instant) -> Option<Duration> {
    Some(deadline.saturating_duration_since(Instant::now()))
}

/// Names the registry entry a `Kill` raiser targets.
#[derive(Clone, Copy)]
pub(crate) enum KillKey<'a> {
    Id(&'a ActorId),
    Name(&'a str),
}

/// What [`raise_kill_declared`] did.
pub(crate) struct KillRaised {
    /// The lane's answer to the `Kill`.
    pub(crate) result: RaiseResult,
    /// The abort handle the registry held after the raise, when the raise was
    /// accepted and `system` was given: absent in the window before the task's
    /// handle is attached, and for an incarnation already gone.
    pub(crate) abort: Option<AbortHandle>,
}

/// Raises `reason` on `lane` where the crate itself stops an actor (a
/// supervisor's orderly stop, its ladder, the link guard of a child, the cut
/// of a start, the shutdown walk) and queues the events of the raise: the
/// request with the lane's own answer and, for a `Kill` the lane took, its
/// `Raised` step. Called with no registry guard or other lock held, since the
/// events run no user code but resolve the actor's identity from the
/// registry. With no sink and no identity recorded it is the raise alone.
/// Returns the lane's answer.
///
/// `origin` is where the request came from: the crate's own stop, a
/// caller's (the actor's own `ctx.stop`), or the failure of an actor whose
/// signal map panicked.
#[inline]
pub(crate) fn raise_noted(
    system: Option<&ActorSystem>,
    lane: &StopLane,
    reason: StopReason,
    origin: crate::telemetry::StopOrigin,
) -> RaiseResult {
    if !crate::telemetry::engaged_for(lane.abort_record(), system) {
        return lane.try_raise(reason);
    }
    raise_noted_engaged(system, lane, reason, origin)
}

/// The part of [`raise_noted`] that runs only while telemetry is on.
#[inline(never)]
fn raise_noted_engaged(
    system: Option<&ActorSystem>,
    lane: &StopLane,
    reason: StopReason,
    origin: crate::telemetry::StopOrigin,
) -> RaiseResult {
    let result = lane.try_raise(reason.clone());
    if matches!(reason, StopReason::Kill) && result != RaiseResult::Refused {
        push_kill_step(lane.abort_record(), system, KillStepKind::Raised, || None);
    }
    emit_stop_requested(system, lane, &reason, result, origin, None);
    result
}

/// The one way a `Kill` is raised on an actor the registry holds. In this
/// order, it declares the deadline of the `Kill` on the registry entry `key`
/// names (`window` from now, or the entry's kill grace when `None`), raises
/// the `Kill` on `lane`, and, when the raise is accepted, schedules the abort
/// of the task through the reaper of the entry's own runtime. The entry is
/// matched by `status_rx`, never by the id or name alone, so a later
/// incarnation under the same id or name is neither declared on nor armed. An
/// entry whose abort handle is not attached yet is armed by the attach, after
/// the earliest deadline declared on it.
///
/// With `step` the raise goes through the call's step cell, which counts the
/// raiser in flight first; `between` runs after that and before the raise.
/// Without `step`, `between` runs just before the raise. With no `system`
/// (the caller found none) nothing is declared and nothing is armed: the
/// `Kill` is still raised.
///
/// With a `cause` (and no `step`) the `Kill` is raised on behalf of that
/// reason, an exit signal's: the lane stores it when it holds no `Kill` yet,
/// and the actor ends with it in place of `Kill`. A bounded stop's step cell
/// has no caused form.
///
/// Never call it while holding a registry guard: the declaration takes a
/// write lock on the same shard.
#[allow(clippy::too_many_arguments)]
pub(crate) fn raise_kill_declared(
    system: Option<&ActorSystem>,
    key: KillKey<'_>,
    status_rx: &StatusRx,
    lane: &StopLane,
    window: Option<Duration>,
    step: Option<&StopStep>,
    between: impl FnOnce(),
    cause: Option<StopReason>,
) -> KillRaised {
    debug_assert!(
        cause.is_none() || step.is_none(),
        "a caused Kill has no step cell form"
    );
    if let Some(system) = system {
        let declared = match key {
            KillKey::Id(id) => system.declare_kill(id, status_rx, window),
            KillKey::Name(name) => system.declare_kill_by_name(name, status_rx, window),
        };
        // The declaration has returned, so no registry guard is held.
        if declared {
            push_kill_step(
                lane.abort_record(),
                Some(system),
                KillStepKind::Declared,
                || window,
            );
        }
    }
    // The event names the cause a `Kill` for an exit signal carries; it is
    // copied only for such a `Kill`, before the lane takes it.
    let noted_cause = cause.clone();
    let result = match step {
        Some(step) => step.raise_kill(lane, between),
        None => {
            between();
            match cause {
                Some(cause) => lane.try_raise_kill_caused(cause),
                None => lane.try_raise(StopReason::Kill),
            }
        }
    };
    if result != RaiseResult::Refused {
        push_kill_step(lane.abort_record(), system, KillStepKind::Raised, || None);
    }
    emit_stop_requested(
        system,
        lane,
        &StopReason::Kill,
        result,
        crate::telemetry::StopOrigin::Requested,
        noted_cause.as_ref(),
    );
    let abort = match system {
        Some(system) if result != RaiseResult::Refused => {
            #[cfg(test)]
            system.run_after_kill_raise();
            system.arm_raised_kill(key, status_rx, lane.abort_record(), window)
        }
        _ => None,
    };
    KillRaised { result, abort }
}

// ---------------------------------------------------------------------------
// RegistryGuard - drop-based auto-unregister
// ---------------------------------------------------------------------------

pub(crate) struct RegistryGuard {
    system: Arc<ActorSystem>,
    id: ActorId,
    name: Option<Arc<str>>,
    /// The registration this guard owns. Removal on drop is checked against
    /// this sequence (see [`ActorSystem::unregister_by_id`]/
    /// [`ActorSystem::unregister_by_name`]) so a stale teardown - one whose
    /// guard is only dropping now, well after a fresh registration already
    /// reused the same id/name - can never remove that newer registration
    /// out from under it. `by_id` refuses a second live registration of an
    /// id, so the sequence check is defense for a stale guard.
    registration_seq: u64,
    /// A sender of the actor's status plane. Released after `drop`
    /// unregistered the actor: on every path that drops the guard last, the
    /// plane reads as ended only once the name is free.
    _status: Option<StatusTx>,
    /// The actor's hold on its subtree token. Declared last, so it is
    /// released after `drop` unregistered the actor and after the status
    /// sender: the subtree ends only once the name is free.
    _subtree: Option<Arc<SubtreeHold>>,
}

impl RegistryGuard {
    pub(crate) fn new(
        system: Arc<ActorSystem>,
        id: ActorId,
        name: Option<Arc<str>>,
        registration_seq: u64,
    ) -> Self {
        Self {
            system,
            id,
            name,
            registration_seq,
            _status: None,
            _subtree: None,
        }
    }

    /// Makes the guard hold a sender of the actor's status plane as well.
    pub(crate) fn with_status(mut self, status: StatusTx) -> Self {
        self._status = Some(status);
        self
    }

    /// Makes the guard hold the actor's subtree token as well.
    pub(crate) fn with_subtree(mut self, subtree: Arc<SubtreeHold>) -> Self {
        self._subtree = Some(subtree);
        self
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        #[cfg(test)]
        crate::actor::runtime::end_hooks::run(crate::actor::runtime::end_hooks::NAME, &self.id);
        // An incarnation spawned while no sink took events and ending while
        // one does is identified now, while its registry entry still names
        // it: its end and what follows it are events about it.
        if self.system.telemetry_mode().any_armed() {
            if let Some(status) = &self._status {
                self.system
                    .resolve_identity(status.abort_record(), &self.id);
            }
        }
        // The end is recorded for the supervisor, which owes a decision on the
        // death, before the entry leaves the registry and ahead of the
        // runner's join. Read only while turn tracking is on.
        let mode = self.system.telemetry_mode();
        if mode.word() & crate::telemetry::CTL_TRACK_TURNS != 0 {
            if let Some(status) = &self._status {
                let own = crate::telemetry::identity_for(
                    mode,
                    status.abort_record(),
                    Some(&self.system),
                    Some(&self.id),
                );
                if let Some(supervisor) = own.as_deref().and_then(|own| own.parent()) {
                    status.abort_record().mark_guard_ended();
                    supervisor.death_ended(self.registration_seq);
                }
            }
        }
        self.system
            .unregister_by_id(&self.id, self.registration_seq);
        if let Some(name) = &self.name {
            self.system.unregister_by_name(name, self.registration_seq);
        }
        #[cfg(test)]
        crate::actor::runtime::end_hooks::run(
            crate::actor::runtime::end_hooks::NAME_FREED,
            &self.id,
        );
        // The run's memberships go before its status can read `Stopped`: the
        // run is marked ended first and its count read after, and a join
        // counts before it lists, so whichever of a join and this mark comes
        // second sees the other. A run that never joined pays this one swap.
        if let Some(status) = &self._status {
            let record = status.record();
            let joined = record.pg_mark_ended();
            #[cfg(test)]
            crate::actor::runtime::end_hooks::run(
                crate::actor::runtime::end_hooks::PG_MARKED,
                &self.id,
            );
            if joined != 0 {
                self.system
                    .pg_remove_member(&(self.id.clone(), record.incarnation()));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SystemPhase
// ---------------------------------------------------------------------------

/// Lifecycle phase of an [`ActorSystem`]'s registry.
///
/// `Active` accepts registrations. `shutdown`/`shutdown_with` moves the
/// system to `ShuttingDown` immediately and to `Defunct` once the last
/// shutdown in flight has ended, if a shutdown has completed since the system
/// was last `Active`; both reject new registrations with
/// [`SpawnError::SystemShuttingDown`]. Unlike a plain one-way flag, `Defunct`
/// is not permanent: [`ActorSystem::reactivate`] moves it back to `Active`,
/// so a system's name is never poisoned forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemPhase {
    Active,
    ShuttingDown,
    Defunct,
}

impl SystemPhase {
    const fn as_u8(self) -> u8 {
        match self {
            SystemPhase::Active => 0,
            SystemPhase::ShuttingDown => 1,
            SystemPhase::Defunct => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            0 => SystemPhase::Active,
            1 => SystemPhase::ShuttingDown,
            _ => SystemPhase::Defunct,
        }
    }
}

/// Shard count of `ActorSystem::by_name` and `ActorSystem::by_id`.
///
/// A `DashMap` grows one shard at a time: the shard that passes seven eighths
/// of its table doubles it while holding its write lock, so every register,
/// lookup and unregister on that shard waits for the rehash. The default shard
/// count follows the CPUs the process may use (a process limited to one CPU
/// gets 4 shards), which makes each rehash cover a quarter of the registry.
/// A constant count keeps each rehash to `1 / REGISTRY_SHARDS` of the entries
/// whatever the affinity: a table growth holds one shard's write lock while it
/// rehashes only that shard's entries, so a spawn that waits on a rehash waits
/// for a small share of the table, never for the whole of a large one.
/// Registration still goes through one shard's
/// entry API, so the id and name uniqueness checks are unchanged. The count is
/// a power of two, as `DashMap` requires.
const REGISTRY_SHARDS: usize = 256;

/// An empty registry map with `REGISTRY_SHARDS` shards and no preallocated
/// table. An idle `ActorSystem` holds the shard locks and empty shard tables
/// of its two registry maps, a fixed cost at any CPU count.
fn registry_map<K: Eq + std::hash::Hash, V>() -> DashMap<K, V> {
    DashMap::with_capacity_and_shard_amount(0, REGISTRY_SHARDS)
}

// ---------------------------------------------------------------------------
// ActorSystem
// ---------------------------------------------------------------------------

/// A named actor registry with coordinated shutdown.
///
/// An actor reaches only the actors whose handles it holds, except through its
/// system, which knows every registered actor by id; among its calls, `get` and
/// `get_by_id` return a typed handle to a registered actor of the type asked
/// for (`get` only for a named one), `actor_ids`, `registered` and
/// `actor_status` read the registry, `stop` and `kill` act on a named actor,
/// and `kill_by_id` on any registered id.
///
/// A system lives in the process map of named systems ([`create`](Self::create),
/// [`get_named`](Self::get_named), [`all`](Self::all)) until a shutdown of it
/// has completed, and from then on only as long as something else holds it:
/// its user's `Arc`, or an actor of it that has not ended. When the last holder
/// lets go the system is freed and its name is free; its reaper tasks end once
/// their pending entries have fired.
/// [`reactivate`](Self::reactivate) puts a shut-down system back to staying in
/// the map, and the system named `"default"` stays for the life of the
/// process.
///
/// `ActorSystem` is a phone book, not a runtime. It does not create or own a
/// Tokio runtime. Actors spawn on whatever runtime is current via
/// `Handle::try_current()`, and a supervised child on the runtime its
/// builder names with
/// [`on_runtime`](crate::actor::context::ChildSpawnBuilder::on_runtime), which
/// must have the time driver unless it has already ended.
/// A runtime built without the Tokio time driver
/// (`Builder::enable_time` or `enable_all`) runs plain actors (actors with no
/// timer and no supervised child), but timers, the kill grace and every bound
/// on a wait sleep on the driver: see
/// [`SpawnBuilder`](crate::actor::SpawnBuilder) for what each does without
/// it. A spawn with `start_timeout` panics when first polled on a calling
/// runtime without the driver, and `stop_within`, `kill_by_id` and
/// `kill_by_id_with_observer` panic when first polled on one without it or
/// with none current, in each case before anything changes; a call that, when
/// first looked at, has nothing to wait for (a spawn refused for its mailbox
/// capacity, its system or its name, a target that does not take the request,
/// an id nothing holds) answers first,
/// on any runtime, and a name taken between that look and the claim still
/// panics on a runtime without the driver.
/// `shutdown_with` panics at its first wait on a root, after the system has
/// entered shutdown.
/// [`SubtreeObserver::wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended)
/// changes nothing, and on a calling runtime without the driver or with none
/// current it panics at the first moment it needs a timer: the observed actor
/// has ended and let go of everything it held, and an actor below it has not
/// ended. It waits without a timer while the actor still runs or is still
/// letting go of what it held, and answers a subtree that has ended without
/// one. With `panic = "abort"` the entry checks cannot tell whether a runtime
/// has the driver, so the first timer a call uses on a runtime without it ends
/// the process.
pub struct ActorSystem {
    name: String,
    by_name: DashMap<Arc<str>, AnyActorHandle>,
    by_id: DashMap<ActorId, AnyActorHandle>,
    shutdown_policy: ShutdownPolicy,
    /// The system's telemetry gate and sinks.
    telemetry: crate::telemetry::ModeOwner,
    /// `Active` until `shutdown`/`shutdown_with` is called (moves to
    /// `ShuttingDown`, then to `Defunct` once the last shutdown in flight has
    /// ended, if a shutdown has completed since the system was last `Active`).
    /// Every change happens under `registration_gate`'s exclusive side.
    /// `register_actor` rejects every registration attempt in either
    /// non-`Active` phase with `SpawnError::SystemShuttingDown`. Not a one-way
    /// flag:
    /// [`ActorSystem::reactivate`] moves a `Defunct` system back to `Active`.
    phase: AtomicU8,
    /// Makes a registration atomic with every phase change, and counts the
    /// shutdowns in flight: `register_actor` holds the shared side from its
    /// phase check through its inserts, and every phase change takes the
    /// exclusive side. A shutdown raises the count as it leaves `Active` and
    /// lowers it when it ends; the last one to end stores `Defunct` if a
    /// shutdown has completed since the system was last `Active`
    /// (`completed_since_active`), whichever order they ended in.
    /// `reactivate` returns to `Active` only from `Defunct` with no shutdown
    /// in flight. A registration therefore either lands before a shutdown's
    /// phase change, and is in that shutdown's first snapshot, or sees a
    /// phase other than `Active` and is refused, for as long as any shutdown
    /// is in flight.
    registration_gate: std::sync::RwLock<usize>,
    /// Whether a shutdown has completed since the system was last `Active`.
    /// Set by each shutdown that completes, cleared only by `reactivate`, and
    /// read by the shutdown that lowers the count to zero, which then stores
    /// `Defunct`. While it is set the system is never `Active` again without
    /// a `reactivate`, so every root registered before the first of those
    /// shutdowns was stopped by the one that completed. Read and written only
    /// under `registration_gate`'s exclusive side.
    completed_since_active: AtomicBool,
    /// The process-group scopes of this system by name, each made on first
    /// use (see [`pg_scope`](Self::pg_scope)). Never cleared by a shutdown.
    pub(crate) pg_scopes: Mutex<HashMap<Arc<str>, Arc<crate::actor::pg::PgScopeInner>>>,
    /// How many times a run's end looked for its process-group memberships.
    #[cfg(test)]
    pub(crate) pg_sweeps: AtomicU64,
    /// This system's reaper feeds, one per runtime, each spawned lazily on
    /// its runtime's first use and replaced when its task has finished (see
    /// [`reaper_handle`](Self::reaper_handle)).
    reaper: Mutex<HashMap<tokio::runtime::Id, ReaperSlot>>,
    /// The slots every helper task of this system is spawned through (the
    /// reaper, the turn-watchdog sweep, the drain of a closed queue's late
    /// values), one per runtime: each helper's end is recorded there.
    helpers: crate::helper::HelperSlots,
    /// Stuck-turn watchdog threshold, in nanoseconds; `0` means off. The
    /// watchdog bit of the mode word (read by every actor's [`TurnClock`])
    /// follows it, so a change here takes effect for already-spawned actors
    /// without any extra signaling. See
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    turn_warning_threshold_nanos: Arc<AtomicU64>,
    /// Runs once between a system-side kill's lookup and its raise, so a test
    /// can store the target's abort handle at that exact point.
    #[cfg(test)]
    after_kill_lookup: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Runs once between a system-side kill's raise and its re-read of the
    /// registry, so a test can delay that re-read.
    #[cfg(test)]
    after_kill_raise: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Runs once right after a registry `Kill`'s deadline is declared, so a
    /// test can read the entry and the actor's lane at that exact point.
    #[cfg(test)]
    after_kill_declaration: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Runs at each point where a shutdown is about to act on one root, so a
    /// test can change the registry at that exact point. Kept until cleared.
    #[cfg(test)]
    ladder_hook: Mutex<Option<LadderHook>>,
    /// How many times `arm_kill_backstop` ran, for the tests that pin which
    /// paths arm it.
    #[cfg(test)]
    backstop_arms: AtomicU64,
    /// Runs once inside a registration, between its phase check and its
    /// inserts.
    #[cfg(test)]
    after_phase_check: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Runs once inside `is_quiescent`, between its two reads.
    #[cfg(test)]
    between_quiet_passes: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once inside a snapshot, after the registry walk and before the
    /// rows are built.
    #[cfg(test)]
    after_snapshot_walk: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// How many identity passes walked the registry.
    #[cfg(test)]
    identity_walks: std::sync::atomic::AtomicUsize,
    /// Runs once in the sweep task, after it decided to end and before it
    /// clears its latch, so a test can land a want in that window.
    #[cfg(test)]
    before_sweep_latch_clear: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Set once a shutdown has taken its last root snapshot.
    #[cfg(test)]
    root_snapshots_done: AtomicBool,
    /// Set by a shutdown just before it takes the registration gate's
    /// exclusive side.
    #[cfg(test)]
    shutdown_at_the_gate: AtomicBool,
    /// Guards the turn-watchdog sweep task's lazy spawn: `true` while a
    /// sweep task is currently alive for this system, `false` otherwise.
    /// Shared (via a clone) with the sweep task itself, whose own drop guard
    /// clears it - back to `false` the moment the task returns OR the
    /// runtime it was spawned on is dropped out from under it - so a system
    /// that outlives its sweep's runtime can always spawn a fresh one on the
    /// next runtime that becomes current, instead of believing forever that
    /// a sweep is still running.
    sweep_spawned: Arc<AtomicBool>,
    /// The runtimes the sweep's spawn found without the time driver, so a
    /// spawn on such a runtime does not probe it again. Bounded: cleared when
    /// full.
    timerless_runtimes: Mutex<std::collections::HashSet<tokio::runtime::Id>>,
    /// The earliest global deadline of a final sweep that started, held while
    /// that sweep runs and after its shutdown completed or was cut short: its
    /// deadline entries stand, so every sweep, one already waiting included,
    /// bounds and reports its leftovers by it. A watch, so a sweep's waits see
    /// it lowered while they run. Cleared when the last shutdown in flight
    /// ends, provided a shutdown has completed since the system was last
    /// `Active`.
    sweep_deadline: watch::Sender<Option<Instant>>,
    /// The instant `AnyActorHandle::kill_due` counts from.
    epoch: Instant,
    /// This system's identity for the life of the process, from
    /// `NEXT_GENERATION`: the slot in the process map, a handle's equality
    /// and the stop paths of a handle all name a system by it.
    generation: u64,
    /// A weak reference to this system's own allocation, for the code that
    /// holds `&self` and needs the shared pointer (`reactivate`'s slot, the
    /// turn-watchdog sweep).
    me: Weak<ActorSystem>,
    /// Runs at the top of `Drop`, before the system's slot is removed, so a
    /// test can claim the name at that exact point.
    #[cfg(test)]
    before_slot_removal: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once inside the registration gate, right after the move to
    /// `Defunct` took the map's strong reference out of the slot, so a test
    /// can act at that exact point.
    #[cfg(test)]
    after_demotion: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl std::fmt::Debug for ActorSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorSystem")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Drop for ActorSystem {
    /// Removes this system's own slot from the process map, exact to its
    /// generation: a later system that claimed the name after this one's last
    /// holder let go keeps its slot. The fields drop after this body: the
    /// reaper feeds close (each reaper fires what it holds and ends), the
    /// helper slots and both registries are freed.
    fn drop(&mut self) {
        #[cfg(test)]
        {
            let hook = self
                .before_slot_removal
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let generation = self.generation;
        let removed = systems().remove_if(&self.name, |_, slot| slot.generation == generation);
        // The guard is gone; the slot (a dead `Weak`) is freed here.
        drop(removed);
    }
}

impl ActorSystem {
    // - Construction -------------------------------------------------------

    fn empty(
        name: String,
        shutdown_policy: ShutdownPolicy,
        turn_warning_threshold: Option<Duration>,
        turn_tracking: bool,
        wait_cycle_detection: bool,
        me: Weak<ActorSystem>,
    ) -> Self {
        #[cfg(all(feature = "tracing", panic = "abort"))]
        {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                caught_log!(warn,
                    "panic = \"abort\": actor panics terminate the process; panic capture and crash-visible supervision are unavailable"
                );
            });
        }

        let telemetry = crate::telemetry::ModeOwner::bound(me.clone(), &name);
        telemetry.0.set_ctl(
            crate::telemetry::CTL_WATCHDOG,
            threshold_to_nanos(turn_warning_threshold) != 0,
        );
        telemetry
            .0
            .set_ctl(crate::telemetry::CTL_TRACK_TURNS, turn_tracking);
        telemetry
            .0
            .set_ctl(crate::telemetry::CTL_DEADLOCK, wait_cycle_detection);
        // The registry is empty: a switch turned on here owes no pass.
        telemetry.0.mark_bound();
        Self {
            name,
            by_name: registry_map(),
            by_id: registry_map(),
            shutdown_policy,
            helpers: crate::helper::HelperSlots::new(telemetry.0.clone()),
            telemetry,
            phase: AtomicU8::new(SystemPhase::Active.as_u8()),
            registration_gate: std::sync::RwLock::new(0),
            completed_since_active: AtomicBool::new(false),
            pg_scopes: Mutex::new(HashMap::new()),
            #[cfg(test)]
            pg_sweeps: AtomicU64::new(0),
            reaper: Mutex::new(HashMap::new()),
            turn_warning_threshold_nanos: Arc::new(AtomicU64::new(threshold_to_nanos(
                turn_warning_threshold,
            ))),
            #[cfg(test)]
            after_kill_lookup: Mutex::new(None),
            #[cfg(test)]
            after_kill_raise: Mutex::new(None),
            #[cfg(test)]
            after_kill_declaration: Mutex::new(None),
            #[cfg(test)]
            ladder_hook: Mutex::new(None),
            #[cfg(test)]
            backstop_arms: AtomicU64::new(0),
            #[cfg(test)]
            after_phase_check: Mutex::new(None),
            #[cfg(test)]
            between_quiet_passes: Mutex::new(None),
            #[cfg(test)]
            after_snapshot_walk: Mutex::new(None),
            #[cfg(test)]
            identity_walks: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            before_sweep_latch_clear: Mutex::new(None),
            #[cfg(test)]
            root_snapshots_done: AtomicBool::new(false),
            #[cfg(test)]
            shutdown_at_the_gate: AtomicBool::new(false),
            sweep_spawned: Arc::new(AtomicBool::new(false)),
            timerless_runtimes: Mutex::new(std::collections::HashSet::new()),
            sweep_deadline: watch::channel(None).0,
            epoch: Instant::now(),
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            me,
            #[cfg(test)]
            before_slot_removal: Mutex::new(None),
            #[cfg(test)]
            after_demotion: Mutex::new(None),
        }
    }

    /// A new system in its own allocation, which knows its own `Weak`.
    fn build(
        name: String,
        shutdown_policy: ShutdownPolicy,
        turn_warning_threshold: Option<Duration>,
        turn_tracking: bool,
        wait_cycle_detection: bool,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| {
            Self::empty(
                name,
                shutdown_policy,
                turn_warning_threshold,
                turn_tracking,
                wait_cycle_detection,
                me.clone(),
            )
        })
    }

    /// Puts a new system under `name` in the process map, if the name is free
    /// or its slot is dead. `Err` carries the live occupant when the map holds
    /// it strongly (and `None` when it holds it only weakly), and nothing is
    /// built then.
    ///
    /// The new system is built under the entry's lock and escapes by return;
    /// a dead slot it replaces holds no live `Arc`, so dropping it under the
    /// lock drops nothing that takes it.
    fn claim(
        name: String,
        build: impl FnOnce(String) -> Arc<ActorSystem>,
    ) -> Result<Arc<ActorSystem>, Option<Arc<ActorSystem>>> {
        use dashmap::mapref::entry::Entry;
        match systems().entry(name.clone()) {
            Entry::Vacant(v) => {
                let system = build(name);
                v.insert(Slot::new(&system));
                Ok(system)
            }
            Entry::Occupied(mut o) => {
                if o.get().is_live() {
                    return Err(o.get().strong.clone());
                }
                let system = build(name);
                let dead = o.insert(Slot::new(&system));
                debug_assert!(dead.strong.is_none() && dead.weak.strong_count() == 0);
                drop(dead);
                Ok(system)
            }
        }
    }

    /// Returns the default system (named `"default"`), creating it lazily.
    ///
    /// The default system is the one ambient system bare spawns join, and it
    /// stays in the process map of named systems for the life of the process,
    /// shut down or not.
    ///
    /// This is intentionally not `std::default::Default` because it returns
    /// `Arc<ActorSystem>` (shared ownership), not an owned value.
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> Arc<ActorSystem> {
        // The ambient system never leaves the map of named systems, and its
        // slot is always strong, so the system the map answers once is the
        // answer for the life of the process. Kept here, a bare spawn skips
        // the hash of the name and the map's shard lock.
        static AMBIENT: OnceLock<Arc<ActorSystem>> = OnceLock::new();
        if let Some(system) = AMBIENT.get() {
            return system.clone();
        }
        let system = Self::ambient();
        AMBIENT.get_or_init(|| system.clone()).clone()
    }

    /// The ambient system as the map holds it, created on first use.
    fn ambient() -> Arc<ActorSystem> {
        loop {
            if let Some(found) = Self::get_named(DEFAULT_SYSTEM) {
                return found;
            }
            let made = Self::claim(DEFAULT_SYSTEM.to_string(), |name| {
                Self::build(name, ShutdownPolicy::default(), None, false, false)
            });
            match made {
                Ok(system) | Err(Some(system)) => return system,
                // The slot is weak and its system is still alive for another
                // thread only: look again.
                Err(None) => std::thread::yield_now(),
            }
        }
    }

    /// Creates a new named system and registers it in the global systems map.
    ///
    /// A system stays in the map from its creation until a shutdown of it
    /// has completed; from then on the map holds it only as long as someone
    /// else does, and when the last holder lets go (the caller's `Arc` and
    /// every actor of the system that has not ended) the system is freed, its
    /// name leaves the map and a later `create` can claim the name again.
    /// [`reactivate`](Self::reactivate) puts a shut-down system back to
    /// staying in the map. The system named `"default"` is the exception: it
    /// stays in the map for the life of the process.
    ///
    /// Returns [`SpawnError::SystemNameTaken`] if a system with this name
    /// already exists.
    pub fn create(name: impl Into<String>) -> Result<Arc<ActorSystem>, SpawnError> {
        let name = name.into();
        Self::claim(name.clone(), |name| {
            Self::build(name, ShutdownPolicy::default(), None, false, false)
        })
        .map_err(|_| SpawnError::SystemNameTaken(name))
    }

    /// Creates a new named system with custom configuration.
    ///
    /// The system's life in the map of named systems is the one
    /// [`create`](Self::create) describes.
    ///
    /// Returns [`SpawnError::SystemNameTaken`] if a system with this name
    /// already exists.
    pub fn create_with(
        name: impl Into<String>,
        config: SystemConfig,
    ) -> Result<Arc<ActorSystem>, SpawnError> {
        let name = name.into();
        Self::claim(name.clone(), |name| {
            Self::build(
                name,
                config.shutdown_policy,
                config.turn_warning_threshold,
                config.turn_tracking,
                config.wait_cycle_detection,
            )
        })
        .map_err(|_| SpawnError::SystemNameTaken(name))
    }

    /// Looks up a named system. Returns `None` if no system with this name
    /// exists, which includes a system that has completed a shutdown and has
    /// no holder left (see [`create`](Self::create)).
    pub fn get_named(name: &str) -> Option<Arc<ActorSystem>> {
        // The slot's own `Arc` escapes by return. A weakly held system is
        // upgraded only after the map's guard has ended, since the upgrade
        // can make this call the last holder and the system's drop takes the
        // same shard.
        let weak = {
            let slot = systems().get(name)?;
            if let Some(strong) = &slot.strong {
                return Some(strong.clone());
            }
            slot.weak.clone()
        };
        weak.upgrade()
    }

    /// Lists the names of all registered systems: the systems of the map that
    /// can still be reached (see [`get_named`](Self::get_named)).
    pub fn all() -> Vec<String> {
        systems()
            .iter()
            .filter(|e| e.value().is_live())
            .map(|e| e.key().clone())
            .collect()
    }

    /// Returns this system's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    // - Actor lookup -------------------------------------------------------

    /// Looks up a named actor, returning a typed handle.
    ///
    /// Returns `None` if the name is not registered (as Erlang/OTP's `whereis/1`
    /// returns `undefined`) or if the registered actor is of a different type.
    ///
    /// A handle is bound to one incarnation of the actor: after a supervised
    /// restart the name resolves to the next incarnation's handle, so a
    /// caller that talks to a restartable actor looks it up by name for each
    /// call rather than keeping a handle. The name stays registered to an
    /// incarnation until its teardown ends, including the time after its
    /// message loop ended, when the handle's
    /// [`is_alive`](ActorHandle::is_alive) reads `false` and a send returns
    /// `Closed`; a caller that meets that window waits with
    /// [`wait_stopped`](ActorHandle::wait_stopped) on the handle it holds and
    /// looks the name up again. A lookup right after a crashing request was
    /// answered (`AskError::Actor`) never finds that incarnation: the answer
    /// is sent once the name is free. A lookup that finds nothing while a
    /// supervisor restarts the actor reads as Erlang/OTP's `undefined` in the
    /// same window; retry after a short delay. For an actor that nothing
    /// restarts, `None` is final.
    pub fn get<A: Actor>(&self, name: &str) -> Option<ActorHandle<A>> {
        self.by_name.get(name).and_then(|e| e.downcast::<A>())
    }

    /// Looks up an actor by [`ActorId`], returning a typed handle.
    ///
    /// Returns `None` if the ID is not registered or if the registered actor is
    /// of a different type, the same contract as [`get`](Self::get), keyed by
    /// ID instead of by name.
    /// To reach an actor by a key of your own, as Erlang/OTP's `via` registries
    /// do, keep a map from that key to its `ActorId` and resolve it with this
    /// call.
    ///
    /// An `ActorId` is stable across restarts: a supervised child keeps the
    /// same ID from incarnation to incarnation even though its `ActorHandle`
    /// is replaced underneath it, so an ID captured once (for example from
    /// [`ChildInfo::id`](crate::types::ChildInfo)) names the child for the
    /// lifetime of its spec; a lookup answers `None` while no incarnation of it
    /// is registered.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult, ActorSystem};
    ///
    /// #[derive(Default)]
    /// struct Counter(i64);
    ///
    /// impl Actor for Counter {
    ///     type Message = i64;
    ///     type Response = i64;
    ///     async fn handle(&mut self, msg: i64, _ctx: &mut ActorContext<Self>) -> ActorResult<i64> {
    ///         self.0 += msg;
    ///         Ok(self.0)
    ///     }
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let handle = Counter::default().spawn().named("counter").await?;
    ///     let id = handle.id().clone();
    ///
    ///     let sys = ActorSystem::default();
    ///     let by_id = sys.get_by_id::<Counter>(&id).expect("registered above");
    ///     assert_eq!(by_id.send(1).await?, 1);
    ///     Ok(())
    /// }
    /// ```
    pub fn get_by_id<A: Actor>(&self, id: &ActorId) -> Option<ActorHandle<A>> {
        self.by_id.get(id).and_then(|e| e.downcast::<A>())
    }

    /// Stops a named actor gracefully.
    ///
    /// Delivered through the actor's stop lane (see the `runtime` module
    /// docs): a synchronous signal that never waits for capacity, observed
    /// ahead of the system channel and the mailbox alike, at the actor's next
    /// turn boundary.
    ///
    /// # Errors
    /// - [`SendError::NotFound`] if no actor is registered under `name`,
    ///   which includes an actor whose task has ended.
    /// - [`SendError::Closed`] if an actor is registered under `name` but no
    ///   longer takes the request (its message loop has ended).
    pub async fn stop(&self, name: &str) -> Result<(), SendError> {
        // Clone the lane out of the `Ref` and drop the guard before using
        // it: holding a `DashMap` guard across an await point can deadlock
        // against a concurrent writer on the same shard (the raise itself is
        // synchronous, but this keeps the same discipline as every other
        // method here).
        let lane = {
            let entry = self.by_name.get(name).ok_or(SendError::NotFound)?;
            entry.stop_lane.clone()
        };
        let raised = lane.try_raise(StopReason::Graceful);
        emit_stop_requested(
            Some(self),
            &lane,
            &StopReason::Graceful,
            raised,
            crate::telemetry::StopOrigin::Requested,
            None,
        );
        match raised {
            RaiseResult::Refused => Err(SendError::Closed),
            RaiseResult::Recorded | RaiseResult::Superseded => Ok(()),
        }
    }

    /// Force-kills a named actor: `pre_stop` cannot veto it, and no lifecycle
    /// callback begins once the actor reads the `Kill`; one already running
    /// finishes, or is aborted when it next yields once the kill grace has
    /// passed (at once when the actor's runtime has no time driver); one that
    /// never yields survives the abort.
    ///
    /// Same stop-lane delivery as [`stop`](Self::stop), at `Kill` severity.
    /// The actor leaves its message loop at its next turn boundary; if it
    /// does not reach one, its task is aborted once its kill grace has
    /// passed (120 ms unless its spawn builder set another), so a handler
    /// parked at an await is aborted after that grace. The abort is
    /// scheduled by the system: dropping this future never cancels it. While
    /// the actor is registered the `Kill` is accepted, including while it
    /// runs `on_stopped` and while its children are being stopped. The name
    /// leaves the registry once the actor's children are stopped (an actor
    /// ending by `Kill` does not wait for them: as its links drop, a worker
    /// child that does not trap exit signals is raised `Kill` and a supervisor
    /// child or a worker that traps them `ParentKilled`, which can come after
    /// the name has left), before its
    /// stop lane is sealed and before its queued messages and its value are
    /// dropped (an aborted actor's when its task ends); from then on this
    /// answers `NotFound`. A `Kill` accepted after the actor left the
    /// registry arms no abort: its one remaining await is the end of its
    /// already aborted timer and stream tasks, which waits on their drops, so
    /// its end never precedes theirs.
    ///
    /// # Errors
    /// - [`SendError::NotFound`] if no actor is registered under `name`,
    ///   which includes an actor whose task has ended.
    /// - [`SendError::Closed`] if the actor stopped taking the request
    ///   between this call's lookup of `name` and its raise: it left the
    ///   registry and sealed its lane, or its task ended. Nothing is aborted
    ///   then.
    pub async fn kill(&self, name: &str) -> Result<(), SendError> {
        // Same guard-before-use discipline as `stop`.
        let (lane, status_rx) = {
            let entry = self.by_name.get(name).ok_or(SendError::NotFound)?;
            (entry.stop_lane.clone(), entry.status_rx.clone())
        };
        #[cfg(test)]
        self.run_after_kill_lookup();
        // Only the incarnation the Kill was raised on is armed: a later
        // registration under the same name is not.
        let raised = raise_kill_declared(
            Some(self),
            KillKey::Name(name),
            &status_rx,
            &lane,
            None,
            None,
            || {},
            None,
        );
        if raised.result == RaiseResult::Refused {
            return Err(SendError::Closed);
        }
        Ok(())
    }

    /// Lists all registered actor names in this system.
    pub fn registered(&self) -> Vec<String> {
        self.by_name.iter().map(|e| e.key().to_string()).collect()
    }

    // - Visibility -----------------------------------------------------------

    /// Snapshot of every actor id currently registered in this system,
    /// named or anonymous, however it was spawned; an actor leaves the list
    /// when its registration ends, after its children are stopped (or, when it
    /// ends by `Kill`, without waiting for them: as its links drop, a worker
    /// child that does not trap exit signals is raised `Kill` and a supervisor
    /// child or a worker that traps them `ParentKilled`, which can come after
    /// the id has left) and before it reads `Stopped`. To wait
    /// until the actors below a killed actor have ended, take an observer with
    /// [`observe_subtree`](Self::observe_subtree) before the kill. Erlang/OTP's
    /// `processes/0` lists every process of the
    /// node; here each system lists its own actors.
    pub fn actor_ids(&self) -> Vec<ActorId> {
        self.by_id.iter().map(|e| e.key().clone()).collect()
    }

    /// Reads an actor's current status by id. Returns `None` if the id is
    /// not registered in this system, which includes an actor that has ended,
    /// since an actor leaves the registry before its status reads
    /// [`ActorStatus::Stopped`].
    pub fn actor_status(&self, id: &ActorId) -> Option<ActorStatus> {
        self.by_id.get(id).map(|e| read_status(&e.status_rx))
    }

    /// Returns an observer of the subtree of the actor registered under `id`,
    /// or `None` when no actor is registered under it.
    ///
    /// Take it before the kill it is meant to outlast: an actor that ends by
    /// `Kill` leaves the registry before the actors below it have ended, so a
    /// lookup made afterwards cannot tell an id that was never registered from
    /// a subtree that is still running. [`kill_by_id`](Self::kill_by_id)
    /// reports the killed actor only; the observer's
    /// [`wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended) waits
    /// for the actors below it as well.
    ///
    /// The observer names the incarnation registered under `id` at this call.
    /// A supervised child that restarts by itself between this call and a
    /// separate kill is killed as a later incarnation, which the observer does
    /// not wait for;
    /// [`kill_by_id_with_observer`](Self::kill_by_id_with_observer) takes the
    /// observer and the kill from one lookup.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, ActorSystem, SubtreeEnd,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Boss;
    ///
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.spawn_child(|| Worker).await?;
    ///         Ok(())
    ///     }
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let sys = ActorSystem::create("observe-subtree-example")?;
    ///     let boss = Boss.spawn().on_system(&sys).supervisor().await?;
    ///     let id = boss.id().clone();
    ///
    ///     // Take the observer before the kill, then kill by id.
    ///     let below = sys.observe_subtree(&id).expect("the boss is registered");
    ///     sys.kill_by_id(&id).await;
    ///
    ///     // The report named the boss only; the observer waits for the worker too.
    ///     assert_eq!(below.wait_ended().await, SubtreeEnd::Ended);
    ///     assert!(sys.actor_ids().is_empty());
    ///     Ok(())
    /// }
    /// ```
    pub fn observe_subtree(&self, id: &ActorId) -> Option<SubtreeObserver> {
        self.by_id
            .get(id)
            .map(|e| SubtreeObserver::new(e.status_rx.clone()))
    }

    /// Resolves the key of a monitor target in this system: the registered
    /// actor's run, or the absence of one. A key that matches nothing keeps
    /// the id the key names (a name is its actor's id) and has no run.
    pub(crate) fn resolve_monitor_target(&self, key: MonitorKey<'_>) -> ResolvedTarget {
        let (actor_id, found) = match key {
            MonitorKey::Id(id) => (
                id.clone(),
                self.by_id
                    .get(id)
                    .map(|e| (e.status_rx.clone(), e.stop_lane.clone())),
            ),
            MonitorKey::Name(name) => (
                ActorId::from(name),
                self.by_name
                    .get(name)
                    .map(|e| (e.status_rx.clone(), e.stop_lane.clone())),
            ),
        };
        let (status_rx, lane) = match found {
            Some((rx, lane)) => (Some(rx), Some(lane)),
            None => (None, None),
        };
        ResolvedTarget {
            actor_id,
            incarnation: status_rx.as_ref().map(|rx| rx.record().incarnation()),
            status_rx,
            lane,
        }
    }

    /// Force-kills an actor by id with the same `Kill` as [`kill`](Self::kill):
    /// no lifecycle callback begins once it is read, a callback still awaiting
    /// after the kill grace is aborted (at once when the actor's runtime has no
    /// time driver), and one that never yields survives the abort and is
    /// reported [`StopOutcome::Unresponsive`]. Erlang/OTP's `exit(Pid, kill)`
    /// sends an untrappable exit signal, and the process exits with reason
    /// `killed` whatever it is running (erlang.org/docs/28).
    /// Returns `None` if the id is not registered, or if its actor's lane
    /// refuses the `Kill` (it stopped taking the request between this call's
    /// lookup and its raise, where [`kill`](Self::kill) answers
    /// [`SendError::Closed`]); nothing is aborted then. Otherwise the target
    /// takes the same `Kill`, kill grace and abort as with
    /// [`kill`](Self::kill), and this call, like the shutdown sweep's force,
    /// also waits: the kill grace, then the abort, then a short settle wait,
    /// and it reports the end. The abort is scheduled by the system when the
    /// `Kill` is accepted while the actor is still registered, before this call
    /// waits: dropping the returned future never cancels it. A `Kill` accepted
    /// after the actor left the registry arms no abort: its one remaining await
    /// is the end of its already aborted timer and stream tasks, which waits on
    /// their drops, so its end never precedes theirs.
    ///
    /// The report names the step of that ladder at which the actor ended and
    /// the reason it ended with ([`StopReport`]); an actor that never yields
    /// to the runtime is reported [`StopOutcome::Unresponsive`]. The report
    /// names the killed actor only: the actors below it end after it, as
    /// [`actor_ids`](Self::actor_ids) says. To wait for them too, take an
    /// observer with [`observe_subtree`](Self::observe_subtree) before the kill
    /// and await
    /// [`wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended) after
    /// it, or take both from one lookup with
    /// [`kill_by_id_with_observer`](Self::kill_by_id_with_observer).
    ///
    /// To force a restart of a supervised child, kill it: its supervisor
    /// restarts it per its restart type and charges the restart budget, as
    /// `exit(Child, kill)` does in Erlang/OTP. An
    /// [`ActorContext`](crate::actor::context::ActorContext) has no
    /// `kill_by_id`, but
    /// [`exit`](crate::actor::context::ActorContext::exit) with
    /// [`StopReason::Kill`] kills a registered actor by id (it cannot be
    /// trapped and is backstopped like a kill). A supervisor handler can also
    /// get its system with [`ActorSystem::get_named`](Self::get_named) and call
    /// `kill_by_id` on it, or call
    /// [`stop_within`](crate::actor::handle::ActorHandle::stop_within) with
    /// [`StopReason::Kill`] on a handle it kept.
    /// [`stop_child`](crate::actor::context::ActorContext::stop_child) is
    /// the bounce that charges nothing.
    ///
    /// # Panics
    ///
    /// Panics when it is first polled with no Tokio runtime current, or with a
    /// current runtime that has no time driver (build it with
    /// `Builder::enable_time` or `enable_all`): the call waits out the kill
    /// grace on the calling runtime, and for a registered id it panics before
    /// it raises anything on the target. A current runtime without the driver is
    /// the only one the check refuses: a runtime that is shutting down or has
    /// ended passes it. An id nothing holds, and a target
    /// that does not take a `Kill`, answer `None` on any runtime.
    /// With `panic = "abort"` the entry cannot tell whether the runtime has the
    /// driver, so a call on a runtime without it ends the process at the first
    /// timer it uses.
    pub async fn kill_by_id(&self, id: &ActorId) -> Option<StopReport> {
        self.kill_by_id_taking(id, |_| ())
            .await
            .map(|(report, ())| report)
    }

    /// [`kill_by_id`](Self::kill_by_id), also returning an observer of the
    /// killed incarnation's subtree, taken from the same lookup that picked
    /// the target.
    ///
    /// The observer names exactly the incarnation the report names. Two calls
    /// ([`observe_subtree`](Self::observe_subtree), then `kill_by_id`) look
    /// the id up twice, and a supervised child that restarts by itself between
    /// them is killed as a later incarnation than the one observed. Await the
    /// observer's
    /// [`wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended) to
    /// wait for the actors below the killed actor as well. The call answers
    /// `None` where `kill_by_id` does, and panics where it does.
    ///
    /// # Panics
    ///
    /// As [`kill_by_id`](Self::kill_by_id).
    pub async fn kill_by_id_with_observer(
        &self,
        id: &ActorId,
    ) -> Option<(StopReport, SubtreeObserver)> {
        self.kill_by_id_taking(id, |status_rx| SubtreeObserver::new(status_rx.clone()))
            .await
    }

    /// The body of [`kill_by_id`](Self::kill_by_id) and of
    /// [`kill_by_id_with_observer`](Self::kill_by_id_with_observer): one
    /// lookup, one raise, one wait. `take` is called once, with the status
    /// plane the lookup found, when the `Kill` is accepted and before the wait.
    async fn kill_by_id_taking<T>(
        &self,
        id: &ActorId,
        take: impl FnOnce(&StatusRx) -> T,
    ) -> Option<(StopReport, T)> {
        // Guard-before-await discipline, as in `stop`/`kill`: clone the
        // pieces `force_stop` needs out of the `DashMap` guard before any
        // `.await`.
        let (status_rx, lane, grace) = match self.by_id.get(id) {
            Some(e) => (e.status_rx.clone(), e.stop_lane.clone(), e.kill_grace),
            None => return None,
        };
        // A target that does not take a `Kill` answers `None` without a
        // timer; the timer check comes before the raise.
        if lane.refuses(&StopReason::Kill) {
            emit_stop_requested(
                Some(self),
                &lane,
                &StopReason::Kill,
                RaiseResult::Refused,
                crate::telemetry::StopOrigin::Requested,
                None,
            );
            return None;
        }
        crate::time_probe::require_caller_timers("ActorSystem::kill_by_id");
        #[cfg(test)]
        self.run_after_kill_lookup();
        // A lane that refuses the `Kill` belongs to a task past its last read
        // of it, and the call answers as `kill` does. The wait is the
        // incarnation's own grace, read with the lookup, so it holds after
        // the entry is gone.
        let raised = raise_kill_declared(
            Some(self),
            KillKey::Id(id),
            &status_rx,
            &lane,
            None,
            None,
            || {},
            None,
        );
        if raised.result == RaiseResult::Refused {
            return None;
        }
        let abort = raised.abort;
        let record = lane.abort_record().clone();
        let taken = take(&status_rx);
        let report = force_stop(status_rx, abort, record, grace, StopOutcome::Killed).await;
        Some((report, taken))
    }

    // - Internal registration (used by spawn path) --------------------------

    /// What registering `name` would answer right now for a system that is
    /// not taking registrations or a name already taken, read without
    /// claiming anything. The registration itself decides again.
    pub(crate) fn precheck_spawn(&self, name: Option<&str>) -> Result<(), SpawnError> {
        if SystemPhase::from_u8(self.phase.load(Ordering::Acquire)) != SystemPhase::Active {
            return Err(SpawnError::SystemShuttingDown(self.name.clone()));
        }
        if let Some(n) = name {
            if self.by_name.contains_key(n) {
                return Err(SpawnError::NameTaken {
                    name: n.to_string(),
                    system: self.name.clone(),
                });
            }
        }
        Ok(())
    }

    /// Registers a spawned actor, returning its registration sequence
    /// number: the incarnation number the actor's handle carries, unique in
    /// the process (the source of [`RegistryGuard`]'s seq-checked removal).
    /// `is_root` distinguishes a top-level (`SpawnBuilder`) spawn, which the
    /// system shutdown walks, from a supervised child (`spawn_child`), which
    /// its supervisor's cascade or the final sweep takes down; such a test
    /// child names no supervisor the registry holds.
    ///
    /// Returns [`SpawnError::SystemShuttingDown`] unless the system is
    /// currently `Active`: OTP's application controller rejects new
    /// registrations while stopping, and (unlike a one-way flag) this also
    /// covers a `Defunct` system that has not yet been
    /// [`reactivate`](Self::reactivate)d.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_actor<A: Actor>(
        &self,
        id: &ActorId,
        name: Option<&Arc<str>>,
        handle: &ActorHandle<A>,
        is_root: bool,
        turn_clock: TurnClock,
        kill_grace: Duration,
        runtime: Handle,
    ) -> Result<u64, SpawnError> {
        let seq = handle.incarnation().number();
        // A hand-built child names a supervisor the registry never holds, so
        // its identity, like that of a child whose supervisor has ended, is
        // never guessed.
        let parent = (!is_root).then(|| (ActorId::from(""), crate::telemetry::UNKNOWN_INCARNATION));
        self.register_actor_as(
            id, name, handle, parent, turn_clock, kill_grace, runtime, seq,
        )
    }

    /// Registers the incarnation numbered `seq`, which the caller drew from
    /// [`next_incarnation`](Self::next_incarnation). The number is the
    /// registration's sequence: shutdown stops roots in reverse order of it,
    /// and a stale guard's removal is checked against it. `parent` is `None`
    /// for a root and the spawning supervisor's id and incarnation number for
    /// a supervised child.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_actor_as<A: Actor>(
        &self,
        id: &ActorId,
        name: Option<&Arc<str>>,
        handle: &ActorHandle<A>,
        parent: Option<(ActorId, u64)>,
        turn_clock: TurnClock,
        kill_grace: Duration,
        runtime: Handle,
        seq: u64,
    ) -> Result<u64, SpawnError> {
        // One run has one number: the registration's sequence is the
        // incarnation the handle's record carries.
        debug_assert_eq!(seq, handle.incarnation().number());
        // A handle a unit test built by hand belongs to the system it is
        // registered in; every spawn has recorded its system already, and the
        // record is set once.
        #[cfg(test)]
        if let Some(me) = self.me.upgrade() {
            handle.stop_lane_ref().abort_record().set_system(&me);
        }
        // Held from the phase check through the inserts (see
        // `registration_gate`). Nothing in this section awaits or panics.
        let gate = self
            .registration_gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if SystemPhase::from_u8(self.phase.load(Ordering::Acquire)) != SystemPhase::Active {
            return Err(SpawnError::SystemShuttingDown(self.name.clone()));
        }
        #[cfg(test)]
        {
            let hook = self.after_phase_check.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }

        if let Some(n) = name {
            let entry = self.by_name.entry(Arc::clone(n));
            match entry {
                dashmap::mapref::entry::Entry::Occupied(_) => {
                    return Err(SpawnError::NameTaken {
                        name: n.to_string(),
                        system: self.name.clone(),
                    });
                }
                dashmap::mapref::entry::Entry::Vacant(v) => {
                    v.insert(AnyActorHandle::new(
                        handle,
                        parent.clone(),
                        seq,
                        turn_clock.clone(),
                        kill_grace,
                        runtime.clone(),
                    ));
                }
            }
        }
        match self.by_id.entry(id.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => {
                // A second live registration of one id is refused. The name
                // this call just claimed is released again, so the refusal
                // leaves no trace behind.
                if let Some(n) = name {
                    self.unregister_by_name(n, seq);
                }
                return Err(SpawnError::DuplicateChild(id.clone()));
            }
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(AnyActorHandle::new(
                    handle, parent, seq, turn_clock, kill_grace, runtime,
                ));
            }
        }
        drop(gate);

        // Start the sweep when something asks for it and none is running.
        // The spawn path resolves a runtime via `Handle::try_current()`
        // before ever reaching here, so one is normally current - but
        // `try_current()` is used here too rather than the panicking
        // `Handle::current()`, so a registration reached some other way, with
        // no runtime current, quietly skips the spawn instead of panicking.
        // Whatever asks later makes its own attempt; either way, the next
        // call with a runtime current retries.
        self.ensure_sweep();

        Ok(seq)
    }

    /// Removes the `by_id` entry for `id`, but only if it is still the
    /// registration identified by `registration_seq`. A stale
    /// [`RegistryGuard`] whose drop is only running now - well after a
    /// fresh registration already reused this id - must never remove that
    /// newer entry (see the field doc on `RegistryGuard::registration_seq`).
    pub(crate) fn unregister_by_id(&self, id: &ActorId, registration_seq: u64) {
        self.by_id
            .remove_if(id, |_, entry| entry.registration_seq == registration_seq);
    }

    /// Same seq-checked removal as [`unregister_by_id`](Self::unregister_by_id), for `by_name`.
    pub(crate) fn unregister_by_name(&self, name: &str, registration_seq: u64) {
        self.by_name
            .remove_if(name, |_, entry| entry.registration_seq == registration_seq);
    }

    /// The identity of the incarnation behind `record`, the actor `id`,
    /// recorded now when its spawn recorded none: an incarnation spawned
    /// while no sink took events has an entry in the registry and nothing in
    /// the telemetry table, so the first event about it (or about a child
    /// that names it as supervisor) reads its number and name from its
    /// registry entry and its supervisor from the entry its own entry names
    /// by id and incarnation number, records the result and returns it.
    /// Returns `None` for an incarnation the registry does not hold under
    /// `id` (it ended, or no spawn made it), and for a supervised child whose
    /// supervisor's incarnation the registry does not hold: nothing is
    /// guessed. Every lookup is by id; no registry guard is held when this
    /// returns, so the caller may push at once.
    pub(crate) fn resolve_identity(
        &self,
        record: &AbortRecord,
        id: &ActorId,
    ) -> Option<Arc<crate::telemetry::Identity>> {
        let mode = self.telemetry_mode();
        let key = record.key();
        if let Some(found) = mode.find(key) {
            return Some(found);
        }
        if record.has_stopped() {
            return None;
        }
        let (id, incarnation, parent, _clock, type_name) = self.entry_of_record(key, id)?;
        let name = self
            .by_name
            .get(id.as_str())
            .filter(|entry| entry.stop_lane.abort_record().key() == key)
            .map(|entry| entry.key().clone());
        let parent = match parent {
            None => None,
            Some((parent_id, parent_seq)) => {
                Some(self.resolve_identity_of(&parent_id, parent_seq)?)
            }
        };
        // The wait token is the incarnation number, the same value a spawn
        // that recorded the identity would give, so a wait cycle through an
        // actor spawned before any sink or switch is reported like any other.
        let identity = Arc::new(
            crate::telemetry::Identity::new(id, incarnation, parent, name, None)
                .with_type(type_name),
        );
        Some(mode.bind_if_absent(key, identity))
    }

    /// [`resolve_identity`](Self::resolve_identity) for the live actor
    /// registered under `id`.
    pub(crate) fn resolve_identity_of_id(
        &self,
        id: &ActorId,
    ) -> Option<Arc<crate::telemetry::Identity>> {
        let record = self
            .by_id
            .get(id)
            .map(|entry| entry.stop_lane.abort_record().clone())?;
        self.resolve_identity(&record, id)
    }

    /// [`resolve_identity`](Self::resolve_identity) for the incarnation
    /// numbered `seq` of the actor `id`; `None` when the registry holds
    /// another incarnation under `id`, or none.
    pub(crate) fn resolve_identity_of(
        &self,
        id: &ActorId,
        seq: u64,
    ) -> Option<Arc<crate::telemetry::Identity>> {
        let record = self
            .by_id
            .get(id)
            .filter(|entry| entry.registration_seq == seq)
            .map(|entry| entry.stop_lane.abort_record().clone())?;
        self.resolve_identity(&record, id)
    }

    /// The pass of an engagement: records the identity of every incarnation
    /// the registry holds, once per engagement (see
    /// [`Mode::bind_once`](crate::telemetry::Mode::bind_once)). The entries
    /// are copied out in one walk, each shard's guard dropped once it is
    /// read, and resolved by id after the walk, outside every guard; an
    /// incarnation already recorded costs one lookup of the table. Never call
    /// it under a registry guard or with the registry mutex of the sinks held.
    pub(crate) fn bind_registered(&self) {
        let mode = self.telemetry_mode();
        mode.bind_once(|| {
            #[cfg(test)]
            self.identity_walks.fetch_add(1, Ordering::Relaxed);
            let rows: Vec<(ActorId, AbortRecord)> = self
                .by_id
                .iter()
                .map(|entry| (entry.key().clone(), entry.stop_lane.abort_record().clone()))
                .collect();
            for (id, record) in rows {
                let _ = self.resolve_identity(&record, &id);
            }
        });
    }

    /// How many passes walked the registry for identities.
    #[cfg(test)]
    pub(crate) fn identity_walks(&self) -> usize {
        self.identity_walks.load(Ordering::Relaxed)
    }

    /// The id, incarnation number, supervisor, turn clock id and actor type
    /// name of the registry entry under `id`, when its stop lane's record is
    /// at `key`. The registry guard is dropped before this returns.
    fn entry_of_record(&self, key: usize, id: &ActorId) -> Option<EntryFacts> {
        let entry = self.by_id.get(id)?;
        (entry.stop_lane.abort_record().key() == key).then(|| {
            (
                id.clone(),
                entry.registration_seq,
                entry.parent.clone(),
                entry.turn_clock.id(),
                entry.handle.mail_view().type_name,
            )
        })
    }

    /// The system's telemetry gate and sinks.
    #[allow(dead_code)]
    pub(crate) fn telemetry_mode(&self) -> &Arc<crate::telemetry::Mode> {
        &self.telemetry.0
    }

    /// Draws the next incarnation number: roots, fresh child spawns and
    /// restarts alike, in every system of the process, come from the one
    /// process-wide counter behind [`Incarnation`](crate::types::Incarnation)
    /// (numbers start at 1, so `0` stands for "no incarnation"). A supervisor
    /// draws a child's number before the spawn, for its ledger; the spawn
    /// draws a root's. The same number is the record's incarnation, the
    /// registration's sequence, the turn clock's id and the wait graph's
    /// token, so no two incarnations of any child, under any name, in any
    /// system, are ever equal, and a stale runner completion (from a
    /// superseded instance, including one that shared its predecessor's name
    /// after a terminate/delete/respawn cycle) can never be mistaken for the
    /// fresh one's death.
    pub(crate) fn next_incarnation(&self) -> u64 {
        crate::types::Incarnation::mint().number()
    }

    /// Moves a `Defunct` system (one with no `shutdown`/`shutdown_with` in
    /// flight, after a shutdown completed) back to `Active`, so its name can
    /// be registered into again instead of staying poisoned forever.
    ///
    /// Returns `false` without effect if the system is not currently
    /// `Defunct` - either still `Active` (nothing to reactivate), or
    /// `ShuttingDown` - or if any shutdown is still in flight: reactivating
    /// out from under one would let a registration land after that
    /// shutdown's last snapshot, neither stopped nor refused. A system left
    /// `ShuttingDown` by shutdowns that were all cut short reaches `Defunct`
    /// by calling [`shutdown`](Self::shutdown) again and letting it complete;
    /// once a shutdown has completed since the system was last `Active`, the
    /// system is `Defunct` whenever no shutdown is in flight, however later
    /// calls ended. A reactivated system is held by the process map of named
    /// systems again, as a new system is, until a shutdown of it completes
    /// (see [`create`](Self::create)).
    pub fn reactivate(&self) -> bool {
        // Taken before the gate and the map's shard: the slot takes this
        // reference if it needs it, and what it does not take is dropped
        // after both are released.
        let mut me = self.me.upgrade();
        {
            let in_flight = self
                .registration_gate
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *in_flight != 0
                || SystemPhase::from_u8(self.phase.load(Ordering::Acquire)) != SystemPhase::Defunct
            {
                return false;
            }
            self.completed_since_active.store(false, Ordering::Relaxed);
            self.phase
                .store(SystemPhase::Active.as_u8(), Ordering::Release);
            // The map holds the system strongly again, in the same section
            // that clears the flag.
            self.promote_slot(&mut me);
        }
        drop(me);
        true
    }

    /// Puts the strong reference back into this system's slot of the process
    /// map, taking it from `me`. Called under the registration gate's
    /// exclusive side, in the section that clears `completed_since_active`.
    /// A system whose slot the map does not hold (another generation holds the
    /// name, or none) leaves `me` untouched. The caller drops what is left in
    /// `me` after the gate.
    fn promote_slot(&self, me: &mut Option<Arc<ActorSystem>>) {
        if let Some(mut slot) = systems().get_mut(&self.name) {
            if slot.generation == self.generation && slot.strong.is_none() {
                slot.strong = me.take();
            }
        }
    }

    /// Takes the strong reference out of this system's slot of the process
    /// map, so the map holds the system only weakly, and returns it for the
    /// caller to drop after the gate and the map's shard are released. Called
    /// under the registration gate's exclusive side, in the section that
    /// stores `Defunct`. The `"default"` system is never demoted, and neither
    /// is a system whose slot another generation holds.
    fn demote_slot(&self) -> Option<Arc<ActorSystem>> {
        if self.name == DEFAULT_SYSTEM {
            return None;
        }
        let mut slot = systems().get_mut(&self.name)?;
        if slot.generation != self.generation {
            return None;
        }
        slot.strong.take()
    }

    /// Attaches the task's abort handle to the just-created registry entries.
    /// Called once, immediately after `handle.spawn(...)`: the `AbortHandle`
    /// does not exist before then, so registration (the name-claim) and this
    /// attach step are necessarily two steps, with a brief window between
    /// them where the entry's `abort` is `None` (see [`AnyActorHandle::abort`]).
    ///
    /// A `Kill` raised in that window found no abort handle to schedule: the
    /// caller raises and then reads the entry's abort, this stores the abort
    /// and then reads the lane, and both go through locks, so at least one
    /// of them sees the other's write. When the lane already holds a `Kill`,
    /// this arms the backstop itself, after the earliest deadline a raiser
    /// declared on the entry before its raise (see
    /// [`raise_kill_declared`]), or after the entry's own kill
    /// grace when none did; a deadline already passed arms it at zero.
    ///
    /// Only the entries of this spawn's own registration, `registration_seq`,
    /// take the handle, each checked under the same entry lock as the write:
    /// an entry a later registration made under the same id or name (after
    /// this incarnation ended and freed them) keeps its own abort handle, and
    /// no backstop is armed through it.
    pub(crate) fn attach_abort(
        &self,
        id: &ActorId,
        name: Option<&str>,
        abort: AbortHandle,
        registration_seq: u64,
    ) {
        let mut armed_with: Option<(StopLane, Duration, Handle)> = None;
        if let Some(mut entry) = self.by_id.get_mut(id) {
            if entry.registration_seq == registration_seq {
                entry.abort = Some(abort.clone());
                armed_with = Some((
                    entry.stop_lane.clone(),
                    entry.kill_grace,
                    entry.runtime.clone(),
                ));
            }
        }
        if let Some(name) = name {
            if let Some(mut entry) = self.by_name.get_mut(name) {
                if entry.registration_seq == registration_seq {
                    entry.abort = Some(abort.clone());
                }
            }
        }
        if let Some((lane, grace, runtime)) = armed_with {
            if matches!(lane.reason(), Some(StopReason::Kill)) {
                // The deadlines are read after the lane: every raiser
                // declared its own before its raise, so the earliest of
                // those whose `Kill` this read saw is among them.
                let due = self.declared_kill_due(id, name, registration_seq);
                let after = match due {
                    0 => grace,
                    due => Duration::from_nanos(due.saturating_sub(self.kill_clock())),
                };
                self.arm_kill_backstop(abort, lane.abort_record(), after, &runtime);
            }
        }
    }

    /// The earliest `Kill` deadline declared on the registration
    /// `registration_seq` under `id` or `name`, `0` when none was.
    fn declared_kill_due(&self, id: &ActorId, name: Option<&str>, registration_seq: u64) -> u64 {
        let by_id = self
            .by_id
            .get(id)
            .filter(|entry| entry.registration_seq == registration_seq)
            .map_or(0, |entry| entry.kill_due);
        let by_name = name
            .and_then(|name| self.by_name.get(name))
            .filter(|entry| entry.registration_seq == registration_seq)
            .map_or(0, |entry| entry.kill_due);
        match (by_id, by_name) {
            (0, due) | (due, 0) => due,
            (a, b) => a.min(b),
        }
    }

    /// Nanoseconds from the system's epoch to now, at least `1` so that `0`
    /// stays free for "no deadline".
    fn kill_clock(&self) -> u64 {
        u64::try_from(
            Instant::now()
                .saturating_duration_since(self.epoch)
                .as_nanos(),
        )
        .unwrap_or(u64::MAX)
        .max(1)
    }

    /// Records the deadline of a `Kill` about to be raised on the incarnation
    /// of `id` whose status plane is `status_rx`: `window` from now, or the
    /// incarnation's kill grace when no window is given. The entry keeps the
    /// earliest deadline asked for, whether or not its abort handle is
    /// attached. Only [`raise_kill_declared`] calls this, before it
    /// raises: an attach that finds the `Kill` raised then reads every
    /// deadline declared before that raise. The entry is matched by its
    /// status plane, never by the id alone, and nothing else runs under the
    /// shard's lock.
    fn declare_kill(&self, id: &ActorId, status_rx: &StatusRx, window: Option<Duration>) -> bool {
        let now = self.kill_clock();
        let mut declared = false;
        if let Some(mut entry) = self.by_id.get_mut(id) {
            if entry.status_rx.same_channel(status_rx) {
                entry.declare_kill(now, window);
                declared = true;
            }
        }
        #[cfg(test)]
        self.run_after_kill_declaration();
        declared
    }

    /// [`declare_kill`](Self::declare_kill) for a raiser that found its target
    /// by name.
    fn declare_kill_by_name(
        &self,
        name: &str,
        status_rx: &StatusRx,
        window: Option<Duration>,
    ) -> bool {
        let now = self.kill_clock();
        let mut declared = false;
        if let Some(mut entry) = self.by_name.get_mut(name) {
            if entry.status_rx.same_channel(status_rx) {
                entry.declare_kill(now, window);
                declared = true;
            }
        }
        #[cfg(test)]
        self.run_after_kill_declaration();
        declared
    }

    /// Schedules the abort of a task whose actor was just sent `Kill`: the
    /// reaper of the target's own runtime aborts it once `grace` has passed
    /// unless it has ended by then. The deadline saturates (a caller's
    /// `within` can reach `Duration::MAX`). Never blocks, never depends on
    /// the caller: the entry belongs to the reaper from here, and every end
    /// it can have (fired, or dropped unfired) aborts a task that is still
    /// running.
    ///
    /// `rt` is the runtime stored in the target's registry entry, never the
    /// caller's: an abort takes effect only when the target's own runtime
    /// next polls the task, and a caller on another runtime, or on none,
    /// would park the entry on a reaper nothing runs.
    pub(crate) fn arm_kill_backstop(
        &self,
        abort: AbortHandle,
        record: &AbortRecord,
        grace: Duration,
        rt: &Handle,
    ) {
        #[cfg(test)]
        self.backstop_arms.fetch_add(1, Ordering::SeqCst);
        let entry = ReaperEntry::new(
            saturating_deadline(Instant::now(), grace),
            abort,
            record.clone(),
        );
        self.feed_reaper(rt, entry);
    }

    /// After a `Kill` was raised on the incarnation whose status plane is
    /// `raised_on`: reads its abort handle, kill grace and runtime from the
    /// registry entry `key` names and schedules the abort, after `window`
    /// when given and otherwise after the entry's kill grace. The read comes
    /// after the raise, never before: a handle stored by `attach_abort`
    /// before this read is armed here, and one stored after it is armed by
    /// `attach_abort`, which reads the lane after storing it. The read is
    /// exact to the incarnation (a later registration under the same id or
    /// name is never armed). Returns the abort handle read: absent in the
    /// window before it is stored, and for an incarnation already gone.
    fn arm_raised_kill(
        &self,
        key: KillKey<'_>,
        raised_on: &StatusRx,
        record: &AbortRecord,
        window: Option<Duration>,
    ) -> Option<AbortHandle> {
        let (abort, grace, runtime) = match key {
            KillKey::Id(id) => self.kill_target_for(id, raised_on)?,
            KillKey::Name(name) => {
                let entry = self.by_name.get(name)?;
                if !entry.status_rx.same_channel(raised_on) {
                    return None;
                }
                (entry.abort.clone(), entry.kill_grace, entry.runtime.clone())
            }
        };
        if let Some(abort) = &abort {
            self.arm_kill_backstop(abort.clone(), record, window.unwrap_or(grace), &runtime);
        }
        abort
    }

    /// Sets the hook that runs once after a registry `Kill` is raised and
    /// before its abort is read and armed.
    #[cfg(test)]
    pub(crate) fn set_after_kill_raise_hook(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.after_kill_raise.lock().unwrap() = Some(Arc::new(hook));
    }

    /// Sets the hook that runs once right after a registry `Kill`'s deadline
    /// is declared.
    #[cfg(test)]
    pub(crate) fn set_after_kill_declaration_hook(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.after_kill_declaration.lock().unwrap() = Some(Arc::new(hook));
    }

    #[cfg(test)]
    fn run_after_kill_declaration(&self) {
        let hook = self.after_kill_declaration.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    fn run_after_kill_raise(&self) {
        let hook = self.after_kill_raise.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    pub(crate) fn set_ladder_hook(
        &self,
        hook: impl Fn(LadderPoint, &ActorId) + Send + Sync + 'static,
    ) {
        *self.ladder_hook.lock().unwrap() = Some(Arc::new(hook));
    }

    /// Replaces the reaper of runtime `rt` by a receiver that counts the
    /// entries handed to it.
    #[cfg(test)]
    pub(crate) fn capture_reaper(&self, rt: &Handle) -> mpsc::UnboundedReceiver<ReaperEntry> {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = rt.spawn(std::future::pending::<()>()).abort_handle();
        self.reaper
            .lock()
            .unwrap()
            .insert(rt.id(), ReaperSlot { tx, task });
        rx
    }

    /// The task ids of the reapers this system holds.
    #[cfg(test)]
    pub(crate) fn reaper_task_ids(&self) -> Vec<tokio::task::Id> {
        self.reaper
            .lock()
            .unwrap()
            .values()
            .map(|slot| slot.task.id())
            .collect()
    }

    /// How many runtimes hold a reaper of this system.
    #[cfg(test)]
    pub(crate) fn reaper_slots(&self) -> usize {
        self.reaper.lock().unwrap().len()
    }

    #[cfg(test)]
    pub(crate) fn clear_ladder_hook(&self) {
        *self.ladder_hook.lock().unwrap() = None;
    }

    #[cfg(test)]
    pub(crate) fn run_ladder_hook(&self, point: LadderPoint, id: &ActorId) {
        let hook = self.ladder_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(point, id);
        }
    }

    #[cfg(test)]
    fn run_after_kill_lookup(&self) {
        let hook = self.after_kill_lookup.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Hands `entry` to the reaper of runtime `rt`. The feed takes every
    /// entry, so the grace of the entry's task is never shortened; a closed
    /// feed (its reaper's task is gone) is retried once on the reaper `rt` has
    /// by then, and a second refusal drops the entry, which aborts its task.
    pub(crate) fn feed_reaper(&self, rt: &Handle, entry: ReaperEntry) {
        let Err(mpsc::error::SendError(entry)) = self.reaper_handle(rt).send(entry) else {
            return;
        };
        if let Err(mpsc::error::SendError(entry)) = self.reaper_handle(rt).send(entry) {
            drop(entry);
        }
    }

    /// Returns the abort handle, the kill grace and the runtime of the task
    /// registered under `id`, but only if that registration's status channel
    /// is `status_rx`'s channel, so a caller holding a handle to one
    /// incarnation can never obtain the abort handle of a later one. The
    /// abort is `None` in the window before the task's abort handle is
    /// attached; the grace and the runtime are always there.
    pub(crate) fn kill_target_for(
        &self,
        id: &ActorId,
        status_rx: &StatusRx,
    ) -> Option<(Option<AbortHandle>, Duration, Handle)> {
        let entry = self.by_id.get(id)?;
        if !entry.status_rx.same_channel(status_rx) {
            return None;
        }
        Some((entry.abort.clone(), entry.kill_grace, entry.runtime.clone()))
    }

    // - Stuck-turn watchdog --------------------------------------------------

    /// Sets this system's stuck-turn watchdog threshold: how long a single
    /// callback of an actor may run before it is reported by
    /// [`stuck_actors`](Self::stuck_actors) (and, with the `tracing` feature
    /// enabled, logged with a warning). `None` turns the watchdog off (the
    /// default for every system); `Some(Duration::ZERO)` is treated the same
    /// as `None`, since a threshold of zero would fire on every actor at
    /// every instant.
    ///
    /// Every callback is timed: `pre_start`, `on_started`, `handle`,
    /// `pre_stop`, `on_child_stopped`, `on_stopped`, `on_idle` and an inline
    /// signal map (the map of an exit that rides the priority lane). An actor
    /// stuck in its initialization or inside its stop is reported too, while
    /// its [`actor_status`](Self::actor_status) reads `Initializing`, `Running`
    /// (inside `pre_stop`) or `Stopping`.
    ///
    /// The watchdog times turns only while a threshold is set: while it is `None`,
    /// [`turn_elapsed`](Self::turn_elapsed) returns `None` for every actor
    /// and [`stuck_actors`](Self::stuck_actors) is empty. Setting a
    /// threshold adds one monotonic clock read per callback of every
    /// actor in this system, for as long as the threshold stays set. A turn
    /// already in flight at the moment a threshold is first set is not
    /// timed; the next turn that actor starts is.
    ///
    /// This is observation only: it never interrupts a turn, never raises a
    /// stop, and never changes how a stop resolves.
    /// [`actor_status`](Self::actor_status) still reports the status the
    /// actor holds ([`ActorStatus::Initializing`], [`ActorStatus::Running`] or
    /// [`ActorStatus::Stopping`]) for a wedged actor - `turn_elapsed` and
    /// `stuck_actors`, not `actor_status`, are how to tell. The closest
    /// Erlang/OTP mechanism is `erlang:system_monitor/2` with
    /// `{long_schedule, Time}` (erlang.org/docs/28), which reports a process
    /// that runs at least `Time` wall-clock milliseconds without being
    /// scheduled out, as a message to its monitor process; this watchdog times
    /// each callback, awaits included, and is read on demand through
    /// `turn_elapsed` and `stuck_actors`. Set the threshold well above normal
    /// turn lengths to avoid false positives.
    ///
    /// With the `tracing` feature, setting a threshold also (re)starts the
    /// background task that logs actors over it: it runs on whichever
    /// runtime was current when it was started, provided that runtime has the
    /// Tokio time driver (a call made on one without it starts nothing; with
    /// `panic = "abort"` the task's first timer ends the process instead),
    /// exits when that runtime is dropped out from under it, and is started
    /// again, when a threshold is in effect, by the next call to this method
    /// or the next actor registration made with a runtime current.
    pub fn set_turn_warning_threshold(&self, threshold: Option<Duration>) {
        let nanos = threshold_to_nanos(threshold);
        self.turn_warning_threshold_nanos
            .store(nanos, Ordering::Relaxed);
        // The watchdog bit stays on while the system monitor's `long_schedule`
        // wants turns timed, whatever this threshold says.
        crate::telemetry::monitor::refresh_watchdog(
            &self.telemetry.0,
            &self.turn_warning_threshold_nanos,
        );
        // With no runtime current nothing is spawned yet; the next
        // `register_actor` call (which always has one) tries again.
        self.ensure_sweep();
    }

    /// Returns this system's current stuck-turn watchdog threshold, as set
    /// by [`set_turn_warning_threshold`](Self::set_turn_warning_threshold)
    /// or [`SystemConfig::turn_warning_threshold`]. `None` means the
    /// watchdog is off. Observation only - see
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    pub fn turn_warning_threshold(&self) -> Option<Duration> {
        nanos_to_threshold(self.turn_warning_threshold_nanos.load(Ordering::Relaxed))
    }

    /// Makes `monitor` the actor that is told about long turns and long
    /// mailboxes, as Erlang/OTP's `erlang:system_monitor/2` does for its
    /// monitor process, and returns the settings it replaces. A system has
    /// one monitor. An empty `config` clears it.
    ///
    /// The monitor is told through `map`, which turns each report into a
    /// message of the monitor's own type; the message is offered to the
    /// monitor's mailbox without waiting. A message the full mailbox refuses
    /// is counted in the next
    /// [`SubscriptionNotice::Overflow`](crate::telemetry::SubscriptionNotice::Overflow)
    /// notice the monitor is given, and its report is offered again at the
    /// next look while the turn still runs or the mailbox is still long,
    /// until the monitor takes it; a report the monitor took is not offered
    /// again, and a mailbox whose long report the monitor never took is not
    /// reported short to it. A mailbox already long at the monitor's first
    /// look is reported to it neither long nor short; its next crossing is.
    /// `config` names what the monitor is told about:
    /// [`TurnStuck`](crate::telemetry::EventKind::TurnStuck) for a turn that
    /// has run longer than `long_schedule`, or longer than the system's turn
    /// warning threshold when that is smaller (the event carries the threshold
    /// it passed), and
    /// [`MailboxBacklog`](crate::telemetry::EventKind::MailboxBacklog) for a
    /// mailbox that reached `enable` messages and again when it fell to
    /// `disable`. Both are sampled by a task of the system, so a report comes
    /// some time after the crossing and a mailbox that grew and shrank between
    /// two looks is not reported; a `map` that panics ends the monitor after
    /// one more call with a [`SubscriptionNotice::MapPanicked`](crate::telemetry::SubscriptionNotice::MapPanicked)
    /// notice. The monitor's own turns and mailbox are never reported to it.
    /// Setting a `long_schedule` makes the system time turns, as the turn
    /// warning threshold does, and leaves that threshold and
    /// [`stuck_actors`](Self::stuck_actors) as they were.
    ///
    /// The monitor is cleared when its actor ends or when it is replaced.
    ///
    /// # Errors
    /// - [`MonitorError::ZeroEnable`] and [`MonitorError::DisableNotBelowEnable`]
    ///   for a queue length that cannot work;
    /// - [`MonitorError::MonitorEnded`] when `monitor` has ended or is not an
    ///   actor of this system;
    /// - [`MonitorError::NoTimeDriver`] when no runtime with the time driver
    ///   is current, since the sampling sleeps on it.
    ///
    /// A call that fails leaves the monitor the system had.
    pub fn system_monitor<A: Actor>(
        &self,
        monitor: &ActorHandle<A>,
        map: impl FnMut(Delivery) -> A::Message + Send + 'static,
        config: MonitorConfig,
    ) -> Result<Option<MonitorSettings>, MonitorError> {
        use crate::telemetry::monitor as mon;

        mon::validate(&config)?;
        let config = mon::normalized(config);
        if mon::is_empty(&config) {
            return Ok(self.clear_system_monitor());
        }
        let ended = {
            let status = monitor.status_rx();
            let reason = status.borrow().reason.is_some();
            reason || status.status() == crate::types::ActorStatus::Stopped
        };
        let identity = if ended {
            None
        } else {
            self.resolve_identity(monitor.stop_lane_ref().abort_record(), monitor.id())
        };
        let Some(identity) = identity else {
            return Err(MonitorError::MonitorEnded);
        };
        let rt = match Handle::try_current() {
            Ok(rt) if crate::time_probe::runtime_has_timers(&rt) => rt,
            _ => return Err(MonitorError::NoTimeDriver),
        };

        let mode = self.telemetry_mode();
        let sub = crate::telemetry::subscription::next_id(mode);
        let (inbox, rx) = mon::MonitorInbox::new(identity.actor().clone());
        let cancel = tokio_util::sync::CancellationToken::new();
        // Built before the task is spawned and moved into it, so the monitor
        // is cleared whether the task runs to its end, is aborted, or is
        // dropped unpolled with its runtime.
        let guard =
            mon::MonitorGuard::new(mode.clone(), sub, self.turn_warning_threshold_nanos.clone());
        let previous = mon::install(
            mode,
            MonitorSettings {
                monitor: identity.actor().clone(),
                config,
            },
            sub,
            inbox.clone(),
            cancel.clone(),
            &self.turn_warning_threshold_nanos,
        );
        // The sweep offers the monitor's reports; it starts here when the
        // monitor is the first thing that wants it.
        self.ensure_sweep();
        let handle = monitor.clone();
        // The monitor's record goes with its id: the handle this task holds
        // keeps that record alive as long, and the helper's end is then
        // counted among the monitor's own events.
        drop(self.spawn_helper(
            &rt,
            HelperKind::Monitor,
            Some(crate::helper::HelperOwner::new(
                monitor.id().clone(),
                Some(monitor.stop_lane_ref().abort_record().clone()),
            )),
            |end| end.observe(mon::forward(guard, rx, inbox, map, handle, cancel)),
        ));
        Ok(previous)
    }

    /// The settings of the system's monitor, if it has one.
    pub fn system_monitor_settings(&self) -> Option<MonitorSettings> {
        crate::telemetry::monitor::settings(self.telemetry_mode())
    }

    /// Clears the system's monitor and returns its settings.
    pub fn clear_system_monitor(&self) -> Option<MonitorSettings> {
        crate::telemetry::monitor::clear(self.telemetry_mode(), &self.turn_warning_threshold_nanos)
    }

    /// Turns turn tracking on or off. While it is on, the system counts the
    /// turns of its actors and flags the turn each one is inside, which is
    /// what [`is_quiescent`](Self::is_quiescent) and the `in_turn` field of a
    /// [`snapshot`](Self::snapshot) read. Off by default. Switch it on before
    /// the turns, restart attempts and child deaths it should see: a turn
    /// already in flight when it is switched on is not flagged, and an
    /// attempt or a death that began earlier is not counted.
    pub fn set_turn_tracking(&self, on: bool) {
        self.telemetry
            .0
            .set_ctl(crate::telemetry::CTL_TRACK_TURNS, on);
        self.telemetry.0.engage_pass();
    }

    /// Whether turn tracking is on (see
    /// [`set_turn_tracking`](Self::set_turn_tracking)).
    pub fn turn_tracking(&self) -> bool {
        self.telemetry.0.word() & crate::telemetry::CTL_TRACK_TURNS != 0
    }

    /// Turns wait-cycle detection on or off. While it is on, a send made from
    /// inside an actor's hook that has to wait on another actor is recorded
    /// for as long as it waits, and a cycle of such waits raises one
    /// [`WaitCycle`](crate::telemetry::EventKind::WaitCycle) event. Detection
    /// reports and changes no send. Off by default.
    ///
    /// What is recorded: `send`, `send_priority`, `notify` and
    /// `notify_priority` made from a hook, while they wait for room in the
    /// target's mailbox or priority lane, and while `send` and `send_priority`
    /// wait for the reply. What is not recorded: sends with a deadline
    /// (`send_timeout` and `send_priority_timeout`). A cycle that passes
    /// through such a wait is not reported.
    ///
    /// What detection sees is a send future polled inside a hook: the switch
    /// is read when the send begins, its wait is recorded when the send first
    /// returns pending and ends when the future completes or drops, so poll a
    /// send where you await it (a send future
    /// moved to another task keeps describing the hook it first waited in
    /// until it is dropped). A hook that was already running when the switch
    /// was turned on records nothing for the rest of that turn. Cycles are
    /// searched within one system: waits between actors of different systems
    /// are not joined into a cycle.
    pub fn set_wait_cycle_detection(&self, on: bool) {
        self.telemetry.0.set_ctl(crate::telemetry::CTL_DEADLOCK, on);
        self.telemetry.0.engage_pass();
    }

    /// Whether wait-cycle detection is on (see
    /// [`set_wait_cycle_detection`](Self::set_wait_cycle_detection)).
    pub fn wait_cycle_detection(&self) -> bool {
        self.telemetry.0.word() & crate::telemetry::CTL_DEADLOCK != 0
    }

    /// A read-only record of every actor registered in the system, in the
    /// order they were registered. It asks no actor anything, so a stuck actor
    /// does not delay it; it takes no lock a running actor holds across a
    /// turn and runs no user code. An actor whose end is final is not listed.
    ///
    /// The records are read one after another, each at its own instant, so
    /// the snapshot is not atomic across actors: a queue length and a status
    /// of two actors may belong to different moments. `in_turn` is filled
    /// while [`turn tracking`](Self::set_turn_tracking) is on, a turn warning
    /// threshold is set or the system monitor watches turns, and `waiting_on`
    /// while
    /// [`wait-cycle detection`](Self::set_wait_cycle_detection) is on.
    ///
    /// The coupling counts of a row ([`monitors`](crate::telemetry::ActorRecord::monitors),
    /// [`monitored_by`](crate::telemetry::ActorRecord::monitored_by),
    /// [`links`](crate::telemetry::ActorRecord::links),
    /// [`pg_groups`](crate::telemetry::ActorRecord::pg_groups) and
    /// [`group_monitors`](crate::telemetry::ActorRecord::group_monitors)) are
    /// read from the incarnation's shared block, where the actor records the
    /// lengths of its own monitor, link and group-monitor tables when they
    /// change and where its watchers and its groups keep the other two
    /// counts: they are the values of the last change, as [`get_status`](crate::actor::handle::ActorHandle::get_status)
    /// reports them, without asking the actor.
    pub fn snapshot(&self) -> Vec<crate::telemetry::ActorRecord> {
        use crate::telemetry::{
            ActorRecord, ActorRef, CTL_DEADLOCK, CTL_TRACK_TURNS, CTL_WATCHDOG,
        };

        let mode = self.telemetry_mode();
        let word = mode.word();
        let timed = word & (CTL_WATCHDOG | CTL_TRACK_TURNS) != 0;

        // One walk of the registry. A shard guard is held only while its
        // entries are read, and what is read under it is atomics, the
        // status plane's leaf cell and the mailbox's counters: no user code,
        // no await, no other registry lock.
        let mut seen: Vec<Seen> = Vec::new();
        for entry in &self.by_id {
            let record = entry.stop_lane.abort_record();
            let ended = {
                let value = entry.status_rx.borrow();
                value.reason.is_some()
            };
            seen.push(Seen {
                id: entry.key().clone(),
                seq: entry.registration_seq,
                key: record.key(),
                token: record.token_addr(),
                parent: record.parent_token_addr(),
                clock: entry.turn_clock.id(),
                live: (!ended).then(|| LiveRead {
                    status: entry.status_rx.status(),
                    mail: entry.handle.mail_view(),
                    children: record.children(),
                    in_turn: if timed {
                        entry.turn_clock.snapshot().map(|(_, elapsed)| elapsed)
                    } else {
                        None
                    },
                    suspended: record.sys_bits() & crate::actor::subtree::SYS_SUSPENDED != 0,
                    coupling: crate::telemetry::CouplingInfo::of(record),
                }),
            });
        }
        // Named actors are found in the second map, after the first walk, so
        // every actor the first walk saw is there unless its name was freed.
        // Each name is kept with the registration number of the entry that
        // holds it, never by a record address alone: a freed address can be
        // reused by a new actor before the rows are built.
        let mut names: HashMap<u64, Arc<str>> = HashMap::new();
        for entry in &self.by_name {
            names.insert(entry.registration_seq, entry.key().clone());
        }

        #[cfg(test)]
        {
            let hook = self.after_snapshot_walk.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        // Everything below works on what was copied out: no guard is held.
        seen.sort_by_key(|row| row.seq);
        let by_token: HashMap<usize, usize> = seen
            .iter()
            .enumerate()
            .filter_map(|(at, row)| row.token.map(|token| (token, at)))
            .collect();
        let by_clock: HashMap<u64, usize> = seen
            .iter()
            .enumerate()
            .map(|(at, row)| (row.clock, at))
            .collect();
        let mut refs: Vec<Option<ActorRef>> = vec![None; seen.len()];
        let mut records = Vec::new();
        for at in 0..seen.len() {
            let Some(live) = &seen[at].live else {
                continue;
            };
            // The table is read by a record address that may have been reused
            // since the walk: an identity is kept only when it is the one of
            // this entry's registration, and the row is built from the
            // walk's own copy otherwise.
            let identity = mode
                .find(seen[at].key)
                .filter(|identity| identity.incarnation() == seen[at].seq);
            let actor = match &identity {
                Some(identity) => identity.actor().clone(),
                None => ref_of(&seen, &by_token, &mut refs, at),
            };
            let waiting_on = if word & CTL_DEADLOCK != 0 {
                mode.wait
                    .target_of(seen[at].clock)
                    .and_then(|target| by_clock.get(&target).copied())
                    .map(|target| ref_of(&seen, &by_token, &mut refs, target))
            } else {
                None
            };
            records.push(ActorRecord {
                actor,
                name: names
                    .get(&seen[at].seq)
                    .filter(|name| name.as_ref() == seen[at].id.as_str())
                    .map(|name| name.to_string()),
                status: live.status,
                type_name: live.mail.type_name,
                label: identity
                    .as_ref()
                    .and_then(|identity| identity.label())
                    .map(|label| label.to_string()),
                mailbox_len: live.mail.mailbox_len,
                mailbox_capacity: live.mail.mailbox_capacity,
                priority_len: live.mail.priority_len,
                priority_capacity: live.mail.priority_capacity,
                child_count: live.children,
                in_turn: live.in_turn,
                waiting_on,
                suspended: live.suspended,
                coupling: live.coupling,
            });
        }
        records
    }

    /// Whether the system is quiescent now: no actor is inside a turn, none
    /// has a queued message or is still initializing, and no restart attempt
    /// is in flight. `None` unless [turn tracking](Self::set_turn_tracking) is
    /// on.
    ///
    /// The registry is read twice. The answer is `true` only when both reads
    /// found the same actors with the same turn counts and every actor idle
    /// with empty lanes, so a turn that began and ended between the reads
    /// makes it `false`. It says the system is quiet at the instant of the
    /// call, not that it will stay quiet: an item a stream forwarder holds, a
    /// send that has not begun and a timer that is not yet due are not seen,
    /// and neither is a message taken from a mailbox and not yet inside its
    /// turn (a window of a few instructions), or a child death before the
    /// child's end is recorded.
    ///
    /// A child's death counts from the child's end until the supervisor has
    /// decided on it, and a restart attempt from its start until the
    /// supervisor takes its outcome, so the gap between them is not quiet. A
    /// group restart counts from the supervisor's decision on the group until
    /// the group's last member has its fate, so the span between its stops
    /// and its starts is not quiet either.
    /// Turn tracking must be on before the turns, attempts and deaths the
    /// answer should cover. The answer is `None` when the tracking switch
    /// changed during the call, an off and on again included.
    pub fn is_quiescent(&self) -> Option<bool> {
        use crate::telemetry::CTL_TRACK_TURNS;

        let mode = self.telemetry_mode();
        if mode.word() & CTL_TRACK_TURNS == 0 {
            return None;
        }
        let switches = mode.ctl_changes();
        let first = self.quiet_pass();
        #[cfg(test)]
        {
            let hook = self.between_quiet_passes.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let second = self.quiet_pass();
        // A switch turned off during the call leaves no counters to compare,
        // and one that moved at all (off and on again included) means turns
        // may have run uncounted between the reads.
        if mode.word() & CTL_TRACK_TURNS == 0 || mode.ctl_changes() != switches {
            return None;
        }
        Some(first.idle && first == second)
    }

    /// Runs `hook` once, in the sweep task, after it decided to end and
    /// before it clears its latch.
    #[cfg(test)]
    pub(crate) fn run_before_sweep_latch_clear(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.before_sweep_latch_clear.lock().unwrap() = Some(Arc::new(hook));
    }

    /// Runs `hook` once, inside the next snapshot, after the registry walk and
    /// before the rows are built.
    #[cfg(test)]
    pub(crate) fn run_after_snapshot_walk(&self, hook: impl FnOnce() + Send + 'static) {
        *self.after_snapshot_walk.lock().unwrap() = Some(Box::new(hook));
    }

    /// Runs `hook` once, inside the next [`is_quiescent`](Self::is_quiescent),
    /// between its two reads.
    #[cfg(test)]
    pub(crate) fn run_between_quiet_passes(&self, hook: impl FnOnce() + Send + 'static) {
        *self.between_quiet_passes.lock().unwrap() = Some(Box::new(hook));
    }

    /// One read of what quiescence compares: for every actor whose end is not
    /// final, its registration number, turn count, in-turn flag and lane
    /// lengths, and the restart attempts in flight across them.
    fn quiet_pass(&self) -> QuietPass {
        let mut rows = Vec::new();
        let mut keys = Vec::new();
        let mut idle = true;
        for entry in &self.by_id {
            if entry.status_rx.borrow().reason.is_some() {
                continue;
            }
            let mail = entry.handle.mail_view();
            let in_turn = entry.turn_clock.token() != 0;
            let starting = entry.status_rx.status() == ActorStatus::Initializing;
            idle &= !in_turn && !starting && mail.mailbox_len == 0 && mail.priority_len == 0;
            rows.push((
                entry.registration_seq,
                entry.turn_clock.turns(),
                in_turn,
                mail.mailbox_len,
                mail.priority_len,
            ));
            keys.push((entry.stop_lane.abort_record().key(), entry.registration_seq));
        }
        rows.sort_unstable();
        // Read after the walk, outside every registry guard. A record address
        // may have been reused since the walk, so an identity counts only
        // when it is the one of the registration the walk read.
        let mode = self.telemetry_mode();
        let attempts: u64 = keys
            .into_iter()
            .filter_map(|(key, seq)| {
                mode.find(key)
                    .filter(|identity| identity.incarnation() == seq)
            })
            .map(|identity| {
                u64::from(identity.restarts_in_flight()) + u64::from(identity.deaths_owed())
            })
            .sum();
        let in_flight = attempts;
        QuietPass {
            rows,
            in_flight,
            idle: idle && in_flight == 0,
        }
    }

    /// Attaches a synchronous handler for the telemetry events of `kinds`
    /// under `id`.
    ///
    /// Returns `false`, and attaches nothing, when a handler is already
    /// attached under `id`. The handler takes effect from the next event: an
    /// event being delivered when the call is made goes to the handlers it
    /// started with.
    ///
    /// An actor carries nothing for telemetry until a handler or a
    /// subscription takes events. An actor spawned while one does has its
    /// events raised with its incarnation number and its ancestry. An actor
    /// spawned before that is identified from the system's registry, with
    /// every other actor the registry holds, by the call that attached the
    /// first handler or subscription: the events about it carry its incarnation
    /// number and its ancestry too, and a supervisor is named with its id and
    /// its incarnation number. Such an actor has no recorded birth time, so
    /// its end event reports no lifetime.
    ///
    /// The [`TurnFinished`](crate::telemetry::EventKind::TurnFinished) event of
    /// a `handle` turn that went on, the
    /// [`RunFinished`](crate::telemetry::EventKind::RunFinished) event and the
    /// [`Trace`](crate::telemetry::EventKind::Trace) events of the actor's
    /// turns run the handler on the actor's own task, between turns, while the
    /// helper holds no event about that actor, so a handler that blocks holds that actor's
    /// next take and whatever waits on its task: a stop request is read only
    /// at the turn boundary, and the stop clock counts the handler because the
    /// turn has already been timed. While the helper holds an event about the
    /// actor (its start events after a spawn, the event of a stop request, a
    /// sender wait, a timer tick or a child's death, or a backlog the helper
    /// has not yet delivered), such an event is queued behind it and runs on
    /// the helper, so it never reaches a handler ahead of an event about the
    /// same actor that was raised before it. The turn events of every other
    /// hook, those of a turn after the actor's loop ended or of a turn that
    /// stopped its own actor, and every other kind, the timer tick and the
    /// sender wait included, run the handler on a helper task of the system,
    /// never on the task or thread that raised the event. The one exception is
    /// [`HandlerDetached`](crate::telemetry::EventKind::HandlerDetached), which
    /// reaches the remaining handlers on the thread that ran the handler that
    /// panicked.
    /// [`KindSet::all`](crate::telemetry::KindSet::all) includes the hot
    /// kinds. The handlers attached for one event run one after another, each
    /// call ending before the next begins, and the crate holds no lock while
    /// one runs; a turn or run event delivered on its actor's own task can run
    /// while the helper delivers an event about another actor, so handlers
    /// guard the state they share. On the helper, events reach a handler in
    /// the order they were raised. The helper runs on
    /// the runtime that started it, so a handler that blocks holds a worker
    /// thread of that runtime, delays the events behind it, and on a
    /// `current_thread` runtime stalls every task on it, the actors and the
    /// stop steps included: handlers must not block. The helper's queue holds
    /// 4096 events; what finds it full is dropped and counted, what finds the
    /// system gone is counted too, and so is what is raised inside a handler of
    /// this system. The count is reported to the handlers that
    /// take [`Dropped`](crate::telemetry::EventKind::Dropped) events, and to
    /// each subscription that would have taken the dropped event as
    /// `missed_before` of its next delivery, only while a handler or
    /// subscription still exists to receive it. Events still queued when the
    /// system is dropped are counted as dropped too. `shutdown` does not wait
    /// for delivery; events raised by the last actors reach handlers only while
    /// the helper's runtime is driven. Hand slow work to a
    /// bounded channel with `try_send`, sending the owned copy
    /// [`to_static`](crate::telemetry::TelemetryEvent::to_static) makes, and
    /// count what is dropped:
    ///
    /// ```
    /// use std::sync::atomic::{AtomicU64, Ordering};
    /// use std::sync::Arc;
    /// use tokio_actors::telemetry::{KindSet, TelemetryEvent};
    /// use tokio_actors::ActorSystem;
    ///
    /// let system = ActorSystem::create("exporter-example").unwrap();
    /// let (tx, mut rx) = tokio::sync::mpsc::channel::<TelemetryEvent<'static>>(1024);
    /// let lost = Arc::new(AtomicU64::new(0));
    /// let counted = lost.clone();
    /// system.attach_telemetry("export", KindSet::all(), move |event| {
    ///     if tx.try_send(event.to_static()).is_err() {
    ///         counted.fetch_add(1, Ordering::Relaxed);
    ///     }
    /// });
    /// // A task of the user's reads `rx` and does the slow work there.
    /// # drop(rx.try_recv());
    /// ```
    ///
    /// The helper stays on the runtime that started it. A handler that holds an
    /// `Arc<ActorSystem>` keeps the system, and with it the helper, alive:
    /// detach it first.
    ///
    /// A handler that panics is detached, the panic is contained, the
    /// remaining handlers still run, and one
    /// [`HandlerDetached`](crate::telemetry::EventKind::HandlerDetached) event
    /// goes to the handlers that remain; a handler that attaches another from
    /// that event and panics in turn starts the chain again, one handler per
    /// step. Under `panic = "abort"` there is no containment: a panic of a
    /// handler ends the process, as a panic of any hook does.
    ///
    /// A handler may call `attach_telemetry` and
    /// [`detach_telemetry`](Self::detach_telemetry). An event of this system
    /// raised from inside one of its handlers is skipped and counted as
    /// dropped like any other: it reaches no handler and no subscription, and
    /// the subscriptions that would have taken it read it as `missed_before`.
    /// A handler of this system that raises events of another system does not
    /// make that system skip them.
    pub fn attach_telemetry(
        &self,
        id: &str,
        kinds: crate::telemetry::KindSet,
        handler: impl Fn(&crate::telemetry::TelemetryEvent<'_>) + Send + Sync + 'static,
    ) -> bool {
        let attached =
            crate::telemetry::handlers::attach(&self.telemetry.0, id, kinds, Arc::new(handler));
        // The registry mutex of the sinks is released: an attach that made
        // the system resolve identities records every registered actor's
        // here, on the caller's thread, not on the first event's.
        self.telemetry.0.engage_pass();
        crate::telemetry::subscription::start_sweep_for(&self.telemetry.0, kinds);
        attached
    }

    /// Detaches the handler attached under `id`; `false` when there is none.
    ///
    /// No event delivered after the call reaches the handler, and the state it
    /// captured is dropped once the last call in flight ends; a panic of that
    /// drop is contained. The call does not wait for a handler that is
    /// running.
    ///
    /// An actor that delivered an event on its own task (a turn event, the run
    /// event or a trace entry) keeps a copy of the whole handler list, read
    /// again when it next delivers such an event and dropped at its end. Until then the state a detached
    /// handler captured, of any kind, stays alive, so do not rely on a detach
    /// to close a channel or release a resource the handler captured while
    /// actors are running.
    pub fn detach_telemetry(&self, id: &str) -> bool {
        crate::telemetry::handlers::detach(&self.telemetry.0, id)
    }

    /// How long the actor registered under `id` has been inside its current
    /// callback (`pre_start`, `on_started`, `handle`, `pre_stop`,
    /// `on_child_stopped`, `on_stopped`, `on_idle` or an inline signal map).
    /// `None` if it is between
    /// callbacks, not registered, or this system's threshold is currently
    /// `None` - the watchdog
    /// times turns only while a threshold is set, so this always returns `None` for
    /// every actor while the watchdog is off, regardless of whether a turn
    /// happens to be in flight.
    ///
    /// Observation only - see
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    pub fn turn_elapsed(&self, id: &ActorId) -> Option<Duration> {
        // Checked here, not just inside `TurnClock::begin`: a turn that was
        // already in flight when the threshold was switched off mid-turn
        // keeps a nonzero token (and would keep answering `Some` from
        // `TurnClock::elapsed` alone) until it ends - this system's current
        // threshold, not the actor's own recorded token, is what decides
        // whether that turn is still reported as timed.
        if self.turn_warning_threshold_nanos.load(Ordering::Relaxed) == 0 {
            return None;
        }
        self.by_id.get(id).and_then(|e| e.turn_clock.elapsed())
    }

    /// Actors whose current callback (`pre_start`, `on_started`, `handle`,
    /// `pre_stop`, `on_child_stopped`, `on_stopped`, `on_idle` or an inline
    /// signal map) has run longer than the
    /// configured threshold, longest first. Empty when no threshold is set.
    ///
    /// Observation only - see
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    pub fn stuck_actors(&self) -> Vec<(ActorId, Duration)> {
        let threshold_nanos = self.turn_warning_threshold_nanos.load(Ordering::Relaxed);
        if threshold_nanos == 0 {
            return Vec::new();
        }
        let threshold = Duration::from_nanos(threshold_nanos);

        // Plain iteration, no `.await` anywhere inside: never hold a
        // `DashMap` guard across an await point. `snapshot()` reads the
        // token and the elapsed time from a single atomic load, so a turn
        // ending and a new one starting mid-iteration can never be reported
        // as a hybrid of the two.
        let mut stuck: Vec<(ActorId, Duration)> = self
            .by_id
            .iter()
            .filter_map(|e| {
                let (_, elapsed) = e.turn_clock.snapshot()?;
                (elapsed > threshold).then(|| (e.key().clone(), elapsed))
            })
            .collect();
        stuck.sort_by_key(|(_, elapsed)| std::cmp::Reverse(*elapsed));
        stuck
    }

    /// Starts the sweep on the runtime current now, when something wants it
    /// and none is alive. A call with no runtime current starts nothing; the
    /// next call with one retries.
    #[inline]
    pub(crate) fn ensure_sweep(&self) {
        if self.sweep_wanted() {
            self.ensure_sweep_wanted();
        }
    }

    /// The part of [`ensure_sweep`](Self::ensure_sweep) that runs only when
    /// something wants the sweep, kept out of line so a spawn that nothing
    /// watches tests the want and goes on.
    #[inline(never)]
    fn ensure_sweep_wanted(&self) {
        // The want was published before this call and the latch is read
        // below; a sweep that is ending clears its latch and then reads the
        // want. The fence orders both pairs, so at least one side sees the
        // other and a want is never left with no sweep.
        std::sync::atomic::fence(Ordering::SeqCst);
        if let Ok(rt) = Handle::try_current() {
            self.ensure_sweep_spawned_with(&rt);
        }
    }

    /// Whether anything wants the sweep to run: a sink that takes
    /// [`TurnStuck`](crate::telemetry::EventKind::TurnStuck) or
    /// [`MailboxBacklog`](crate::telemetry::EventKind::MailboxBacklog), an
    /// installed system monitor (it arms no kind: its thresholds are in the
    /// mode), or, with the `tracing` feature, a turn warning threshold. Three
    /// relaxed loads when nothing asks and the feature is off.
    #[inline]
    pub(crate) fn sweep_wanted(&self) -> bool {
        use crate::telemetry::EventKind;
        let mode = self.telemetry_mode();
        let sampled = EventKind::TurnStuck.bit() | EventKind::MailboxBacklog.bit();
        mode.word() & sampled != 0
            || mode.monitor_long_schedule_nanos() != 0
            || mode.monitor_queue() != 0
            || (cfg!(feature = "tracing")
                && self.turn_warning_threshold_nanos.load(Ordering::Relaxed) != 0)
    }

    /// Spawns the turn-watchdog sweep task for this system, unless one is
    /// already alive (`sweep_spawned` is already `true`). A no-op whenever a
    /// sweep is currently running; once that sweep's task ends - see
    /// [`turn_watchdog_sweep`]'s drop guard - the next call here spawns a
    /// fresh one.
    ///
    /// The sweep sleeps on `rt`, so a runtime without the time driver is
    /// never used for it: the call does nothing there, and the next call on a
    /// runtime with the driver starts it. A runtime found without the driver
    /// is remembered by this system, so a spawn there does not probe it again.
    fn ensure_sweep_spawned_with(&self, rt: &Handle) {
        // A sweep that is alive needs no probe. A runtime known to have the
        // driver goes on; one this system already found without it returns;
        // any other is probed, outside every lock, and a negative answer is
        // remembered.
        if self.sweep_spawned.load(Ordering::Acquire) {
            return;
        }
        if !crate::time_probe::runtime_known_to_have_timers(rt) {
            let id = rt.id();
            if self
                .timerless_runtimes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&id)
            {
                return;
            }
            if !crate::time_probe::probe_runtime_for_timers(rt) {
                let mut memo = self
                    .timerless_runtimes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if memo.len() >= TIMERLESS_MEMO_LIMIT {
                    memo.clear();
                }
                memo.insert(id);
                return;
            }
        }
        if self
            .sweep_spawned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // Built here and moved into the spawned future as a plain captured
        // value, not constructed as the first line of the future's own
        // body: a future's captures drop along with it even if it is never
        // polled, so this way the latch clears whether the task runs to
        // completion, is aborted mid-flight, or is dropped unpolled because
        // its runtime never gets around to it before shutting down.
        let guard = SweepLatchGuard(self.sweep_spawned.clone());
        let system = self.me.clone();
        drop(self.helpers.spawn(rt, HelperKind::Sweep, None, |end| {
            end.observe(async move {
                turn_watchdog_sweep(system.clone()).await;
                // The sweep decided to end. A want that arrived since its last
                // look found the latch set and started nothing, so the latch
                // clears first and the want is read again after it: either
                // that caller or this read starts the next sweep.
                #[cfg(test)]
                if let Some(live) = system.upgrade() {
                    let hook = live.before_sweep_latch_clear.lock().unwrap().take();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                drop(guard);
                std::sync::atomic::fence(Ordering::SeqCst);
                if let Some(live) = system.upgrade() {
                    live.ensure_sweep();
                }
            })
        }));
    }

    /// Spawns a helper task of the crate on `rt` through this system's helper
    /// slot, which records the task's end.
    pub(crate) fn spawn_helper<F, Fut>(
        &self,
        rt: &Handle,
        kind: HelperKind,
        owner: Option<crate::helper::HelperOwner>,
        make: F,
    ) -> tokio::task::JoinHandle<()>
    where
        F: FnOnce(crate::helper::EndGuard) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.helpers.spawn(rt, kind, owner, make)
    }

    /// The helper tasks of this system that have started and not ended.
    #[cfg(test)]
    pub(crate) fn live_helpers(&self) -> usize {
        self.helpers.live()
    }

    /// The helper slot of `rt`, if this system has started a helper there.
    #[cfg(test)]
    pub(crate) fn helper_slot_of(&self, rt: &Handle) -> Option<Arc<crate::helper::HelperSlot>> {
        self.helpers.slot_of(rt)
    }

    /// The ends the helper slot of `rt` has recorded, in order.
    #[cfg(test)]
    pub(crate) fn helper_ends(&self, rt: &Handle) -> Vec<(HelperKind, crate::helper::HelperEnd)> {
        self.helpers.ends(rt)
    }

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

    // - Shutdown -------------------------------------------------------------

    /// Shuts down all registered actors using the system's stored policy.
    /// See [`shutdown_with`](Self::shutdown_with); with a root to wait on it
    /// needs a Tokio runtime with the time driver enabled.
    pub async fn shutdown(&self) -> ShutdownReport {
        self.shutdown_with(self.shutdown_policy.clone()).await
    }

    /// Shuts down the system: stops every ROOT actor (one spawned via the
    /// top-level `SpawnBuilder`, not a supervised child) in reverse
    /// registration order, sequentially, escalating from
    /// `StopReason::ParentRequest` up through `Kill` and finally a task abort
    /// as needed. Each root's own supervision cascade takes its subtree down
    /// in turn: the walk takes the roots down their ladders, and every other
    /// actor is stopped by its supervisor (when a supervisor ends by a `Kill`
    /// or an abort, the link guards of its children stop them) or by the
    /// walk's final sweep described below. A supervisor evaluates restarts
    /// only in its own message loop, so once that loop has exited no restart
    /// of its children is begun.
    ///
    /// New registrations are rejected with `SpawnError::SystemShuttingDown`
    /// from the first poll of this call.
    /// The system reads `Defunct` once a shutdown has completed since it was
    /// last `Active` and no shutdown is in flight any more. Shutdowns cut
    /// short before one completes (see "The walk and its caller" below)
    /// leave the system `ShuttingDown`, refusing registrations and with
    /// [`reactivate`](Self::reactivate) returning `false`, until a shutdown
    /// completes; after that the system keeps refusing registrations, and
    /// `reactivate` works again whenever no shutdown is in flight, however
    /// later calls ended.
    ///
    /// Bounded by `policy.timeout`: once the global deadline passes, every
    /// remaining root is force-stopped (`Kill` + `abort`) CONCURRENTLY instead
    /// of sequentially, so this always returns close to the deadline regardless
    /// of how many roots are still outstanding. A final sweep handles anything
    /// still left in the registry afterward (normally nothing: supervised
    /// children vanish via their own guard drops during the cascade above). The
    /// sweep waits on every leftover at once, each wait bounded by the global
    /// deadline, so a supervisor still stopping its children in order after its
    /// own supervisor ended can finish; every leftover still running at the
    /// global deadline is then force-stopped (`Kill` + `abort`) concurrently. A
    /// leftover that nothing stops is therefore held until the global deadline
    /// and then killed. After the global deadline, a shutdown can run on for
    /// the longest kill grace among the roots it forces plus the abort settle
    /// time, then the longest kill grace among the leftovers it forces plus the
    /// abort settle time, and, for either of those two steps, that time once
    /// more plus the abort settle time when the abort of an actor it forces,
    /// scheduled with the system, is issued late. That deadline `Kill`, and the
    /// abort one kill grace after it (counted from when the `Kill` fires), are
    /// scheduled with the system when the sweep begins, on the runtime of each
    /// leftover whose task is already spawned, so they happen even if the walk
    /// is cut short before the deadline. Since a stop only ever strengthens,
    /// the system holds one sweep deadline: a sweep lowers it to its own when
    /// that is earlier, but never to before the moment it lowers it, and every
    /// sweep, one already waiting in another call included, waits on and
    /// reports its leftovers by it. It is cleared only when the last shutdown
    /// in flight ends after a shutdown has completed since the system was last
    /// `Active` (shutdowns that are all cut short before one completes leave
    /// it, as they leave `ShuttingDown`). A shutdown made after only cut-short
    /// ones folds into the stored deadline, which may already have passed, and
    /// then forces its leftovers at once.
    ///
    /// A leftover whose task ended without publishing `Stopped` reads
    /// `Aborted`. One that published `Stopped` reads by the instant its end
    /// became final (its stop lane's seal) against the system's deadline:
    /// `Killed` when that instant is at or after the deadline (or there is
    /// none) or when this shutdown's force took a `Kill` on the leftover, and
    /// `Graceful`, with the reason it ended with, when it is before the
    /// deadline. A leftover reads `Unresponsive` when even the abort did not
    /// end it within the settle time after it. The instant does not
    /// depend on which call looks or when, so concurrent calls report one end
    /// the same way.
    ///
    /// Needs a Tokio runtime with the time driver enabled (`enable_time` or
    /// `enable_all`) that is driven, not only entered: the walk is a task of
    /// the system on that runtime. Without the driver the call panics at its
    /// first wait on a root, after the system has entered shutdown, and a
    /// leftover whose wait fails is forced at once and reported from its
    /// plane; a system with no root completes.
    ///
    /// Registry entries are removed as their actors die (the existing
    /// `RegistryGuard` drop machinery) - never cleared up front, so a lookup
    /// racing the early part of shutdown still finds a live actor.
    ///
    /// # The walk and its caller
    /// The call enters the shutdown, so that registrations are refused, in
    /// its first poll, and then runs the walk in a task of the system on the
    /// runtime that is current at the call; the caller only waits for the
    /// walk's report. The walk therefore survives its caller's future: a call
    /// that is dropped, abandoned, stored and never polled again, or made from
    /// inside an actor that the walk stops, does not stop the walk, which
    /// takes each root down its ladder (a stop request, then a `Kill` once its
    /// time has passed, then an abort; once the global deadline has passed,
    /// the remaining roots are force-stopped with a `Kill` and an abort),
    /// completes, and leaves the system `Defunct` once no other shutdown is in
    /// flight; a root that does not yield even to the abort is reported
    /// `Unresponsive` and may still be running. A call from inside a root is
    /// stopped with that root like any other pending handler, and the walk goes
    /// on to the roots after it. Several calls at once each run their own walk,
    /// and their sweep deadlines fold into one as described above.
    ///
    /// OTP parity: `init:stop/0` is a request to a system process, `init`,
    /// which owns the node's shutdown and returns to the caller at once; here
    /// the walk is likewise a task of the system, and the caller waits for its
    /// report. The first two steps of each root's escalation follow
    /// `shutdown/1` in `supervisor.erl`, which runs in the supervisor process:
    /// a stop request, then a `Kill` (the counterpart of `exit(Pid, kill)`)
    /// once its time has passed. The abort that follows, as the backstop for a
    /// callback that does not reach a turn boundary, has no OTP counterpart,
    /// since `exit(Pid, kill)` needs no cooperation from the process. OTP waits
    /// without limit for the `'DOWN'` after its kill; here the walk settles
    /// after a short time and reports the root `Unresponsive`: a declared
    /// departure.
    ///
    /// The walk does not survive its own runtime. When the runtime it runs on
    /// ends before the walk does and nothing is still waiting on the call, the
    /// walk stops there: it asks no further
    /// root to stop, so the roots on other runtimes that it had not yet
    /// reached keep running (roots on the ended runtime end with it), and the
    /// system keeps refusing
    /// every supervised restart as well as every new registration with
    /// [`SpawnError::SystemShuttingDown`], and a refused restart is charged to
    /// the restart budget like any failed restart: a supervisor whose child
    /// then dies and is due a restart exhausts a budget whose window holds its
    /// `max_restarts` restarts, while a zero window with a `max_restarts`
    /// above zero keeps retrying until a shutdown reaches the supervisor,
    /// unless the restarts read the same instant (see
    /// [`SupervisionConfig::restart_window`](crate::actor::supervision::SupervisionConfig::restart_window)).
    /// [`reactivate`](Self::reactivate) works again once a shutdown of the
    /// system has completed (an earlier one counts) and none is in flight, and
    /// a later call to [`shutdown`](Self::shutdown) can complete one. If the
    /// task ends without a report while its caller still waits (the walk
    /// panicked, or the runtime it ran on ended while the call was polled from
    /// outside it), the caller walks the system itself in its own future and
    /// returns that report if that walk completes; a walk that needs a timer on
    /// an ended runtime panics at its first wait on a root that is still
    /// running. A call made with no Tokio runtime current walks in the
    /// caller's future, and dropping that future stops the walk. With no
    /// runtime current the call completes only for a system with no root and no
    /// leftover: once a root exists, it panics at its first wait on it, because
    /// that wait uses a timer, which needs a driven runtime with the time
    /// driver. The system keeps refusing registrations.
    pub async fn shutdown_with(&self, policy: ShutdownPolicy) -> ShutdownReport {
        #[cfg(test)]
        self.shutdown_at_the_gate.store(true, Ordering::SeqCst);
        let deadline = saturating_deadline(Instant::now(), policy.timeout);
        let Some((rt, owner)) = self.walk_host() else {
            // No runtime is current, or the system is not the one the global
            // map holds: the caller drives the walk.
            let in_flight = ShutdownInFlight::enter(self);
            let roots = self.first_snapshot();
            return self.walk(policy, deadline, roots, in_flight).await;
        };
        // The phase change and the first snapshot happen here, in the
        // caller's first poll: a registration attempted right after it is
        // refused, and every root registered before it is in the snapshot.
        let in_flight = ShutdownInFlight::enter(owner.clone());
        let roots = owner.first_snapshot();
        let (report_tx, report_rx) = oneshot::channel();
        let report_tx = WalkReport(Some(report_tx));
        let walk_policy = policy.clone();
        let walker = owner.clone();
        drop(
            owner.spawn_helper(&rt, HelperKind::ShutdownWalk, None, move |end| {
                end.observe(async move {
                    let report = walker.walk(walk_policy, deadline, roots, in_flight).await;
                    // The helper's hold on the system ends before the caller
                    // is woken, so a caller that drops its own handle right
                    // after the call is the last holder.
                    drop(walker);
                    report_tx.send(report);
                })
            }),
        );
        match report_rx.await {
            Ok(report) => report,
            Err(_) => {
                // The walk helper ended without a report (it panicked, or its
                // runtime ended): the caller walks the system itself.
                let in_flight = ShutdownInFlight::enter(self);
                let roots = self.first_snapshot();
                self.walk(policy, deadline, roots, in_flight).await
            }
        }
    }

    /// The runtime and the owning handle a shutdown walk runs on, or `None`
    /// when no runtime is current or this system is not the one the global
    /// map holds under its name.
    fn walk_host(&self) -> Option<(Handle, Arc<ActorSystem>)> {
        let rt = Handle::try_current().ok()?;
        let held = Self::get_named(&self.name)?;
        (held.generation == self.generation).then_some((rt, held))
    }

    /// The roots a shutdown that has just entered takes on first, in the
    /// order it stops them: the most recently registered first.
    fn first_snapshot(&self) -> Vec<RootSnapshot> {
        let mut roots = self.snapshot_roots();
        roots.sort_by_key(|r| std::cmp::Reverse(r.registration_seq));
        roots
    }

    /// The shutdown walk: stops `roots`, sweeps the leftovers and, when it
    /// completes, releases `in_flight` as a completed shutdown.
    async fn walk<S: std::ops::Deref<Target = ActorSystem>>(
        &self,
        policy: ShutdownPolicy,
        deadline: Instant,
        roots: Vec<RootSnapshot>,
        mut in_flight: ShutdownInFlight<S>,
    ) -> ShutdownReport {
        // Every root incarnation this shutdown has taken on, by its
        // registration sequence (unique to one registration, so to one
        // incarnation): a later incarnation registered under a reported id
        // is a root of its own, not one already reported.
        let mut reported: HashSet<u64> = roots.iter().map(|r| r.registration_seq).collect();
        let mut outcomes = self
            .stop_roots(&roots, policy.per_actor_timeout, deadline)
            .await;

        // Every registration landed before the phase change above or is
        // refused while any shutdown is in flight (see `registration_gate`),
        // so the first snapshot holds every root a public call can produce
        // and this pass finds none through them. It stays as a defensive
        // branch: a registration that bypasses the gate (a test helper) is
        // still stopped. Each pass re-snapshots the roots and stops any root
        // incarnation not yet taken on; it ends once a snapshot holds nothing
        // new.
        loop {
            let mut late: Vec<RootSnapshot> = self
                .snapshot_roots()
                .into_iter()
                .filter(|r| !reported.contains(&r.registration_seq))
                .collect();
            if late.is_empty() {
                break;
            }
            late.sort_by_key(|r| std::cmp::Reverse(r.registration_seq));
            reported.extend(late.iter().map(|r| r.registration_seq));
            outcomes.extend(
                self.stop_roots(&late, policy.per_actor_timeout, deadline)
                    .await,
            );
        }
        #[cfg(test)]
        self.root_snapshots_done.store(true, Ordering::SeqCst);

        // Final sweep: anything still registered that wasn't already taken
        // on above. Normally empty - supervised children vanish via their
        // own guard drops as part of each root's cascade. A leftover can be
        // a supervisor whose own supervisor ended before it did and that is
        // stopping its children in order, or is about to: a killed parent's
        // child is raised its stop only when the runtime drops the runner
        // task its parent aborted, and a grandchild only when its own
        // parent's teardown reaches it, so nothing read here tells a
        // leftover about to stop from one that nothing stops. Every leftover
        // is therefore waited on until the global deadline, since a `Kill`
        // would cut an orderly stop short; those still running then are
        // force-stopped concurrently. These are non-root leftovers, so they
        // are reported in `swept`, never `outcomes`.
        //
        // The deadline's `Kill` and abort do not depend on this future: for
        // each leftover the system schedules, at the snapshot, a two-stage
        // entry on the reaper of the leftover's runtime, built from the
        // registration found (its own lane, task and kill grace, so a later
        // registration under the same id is never touched): `Kill` at the
        // global deadline, the abort one kill grace after it. An entry whose
        // task has ended by a stage does nothing at that stage. The force
        // sweep below still runs at the deadline and reports the outcomes.
        //
        // The deadline belongs to the system: this sweep folds its own into
        // the system's (see `fold_sweep_deadline`), arms and waits by the
        // result, and its waits follow the system's deadline while they run,
        // so a sweep that starts later with an earlier deadline shortens them
        // too. Every ended leftover is classified against the system's
        // deadline by the instant its end became final, which no call's
        // timing changes: one end gets one outcome whichever call reports
        // it.
        let deadline = self.fold_sweep_deadline(deadline);
        let mut deadline_kills: Vec<(Handle, ReaperEntry)> = Vec::new();
        let leftover: Vec<SweepTarget> = self
            .by_id
            .iter()
            .filter(|e| !reported.contains(&e.registration_seq))
            .map(|e| {
                if let Some(abort) = e.abort.clone() {
                    deadline_kills.push((
                        e.runtime.clone(),
                        ReaperEntry::two_stage(
                            deadline,
                            e.stop_lane.clone(),
                            saturating_deadline(deadline, e.kill_grace),
                            abort,
                        ),
                    ));
                }
                SweepTarget {
                    id: e.key().clone(),
                    snapshot: RootSnapshot {
                        id: e.key().clone(),
                        registration_seq: e.registration_seq,
                        status_rx: e.status_rx.clone(),
                        kill_grace: e.kill_grace,
                        record: e.stop_lane.abort_record().clone(),
                    },
                    kill_raised: false,
                }
            })
            .collect();
        for (runtime, entry) in deadline_kills {
            self.feed_reaper(&runtime, entry);
        }
        // Every leftover is waited on at once, each wait bounded by the
        // system's deadline. A wait task that fails (it panics or is
        // cancelled) has its leftover read as not seen to end, so it goes to
        // the force: every leftover is reported once.
        let mut waits = JoinSet::new();
        let mut by_task: HashMap<tokio::task::Id, (usize, SweepTarget)> = HashMap::new();
        for (index, target) in leftover.into_iter().enumerate() {
            let mut rx = target.snapshot.status_rx.clone();
            let mut system_deadline = self.sweep_deadline.subscribe();
            let task = waits.spawn(async move {
                wait_end_by_the_system_deadline(&mut rx, &mut system_deadline, deadline).await
            });
            by_task.insert(task.id(), (index, target));
        }
        let mut waited = Vec::new();
        while let Some(joined) = waits.join_next_with_id().await {
            let (task, ended) = match joined {
                Ok((task, ended)) => (task, ended),
                Err(err) => (err.id(), false),
            };
            if let Some((index, target)) = by_task.remove(&task) {
                waited.push((index, target, ended));
            }
        }
        waited.sort_by_key(|(index, ..)| *index);
        let mut swept: Vec<(ActorId, StopReport)> = Vec::new();
        let mut stragglers: Vec<SweepTarget> = Vec::new();
        for (_, target, ended) in waited {
            // A leftover that ended is reported from the plane it was found
            // with, never looked up by id again (its registry entry may be
            // gone).
            if ended {
                let report = leftover_report(
                    &target.snapshot.status_rx,
                    false,
                    self.current_sweep_deadline(deadline),
                );
                swept.push((target.id, report));
            } else {
                stragglers.push(target);
            }
        }
        if !stragglers.is_empty() {
            swept.extend(
                self.concurrent_force_sweep(stragglers, Some(deadline))
                    .await,
            );
        }

        // Shutdown has fully completed. Every walk snapshots after its own
        // entry, and registrations are refused from the first entry after
        // the system was last `Active`, so this walk has stopped every root
        // registered before that. The last shutdown in flight to end, this
        // one or a cut-short one that ends later, therefore moves the system
        // to `Defunct` rather than leaving `ShuttingDown` set forever, so
        // `reactivate` can return this system's name to service, and clears
        // the system's sweep deadline in the same step.
        in_flight.completed = true;
        drop(in_flight);

        ShutdownReport { outcomes, swept }
    }

    /// Folds a sweep's own deadline into the system's and returns the result,
    /// the deadline this sweep arms and waits by. The system's deadline becomes
    /// the earlier of the stored one and this sweep's own, where this sweep's
    /// own is first raised to an instant just after now: a lowering by this
    /// call then cannot land on an end already final, so an end compared with
    /// the system's deadline before the lowering and after it reads one way. A
    /// stored deadline already past stays as it is.
    fn fold_sweep_deadline(&self, own: Instant) -> Instant {
        let floor = own.max(saturating_deadline(Instant::now(), Duration::from_nanos(1)));
        let mut folded = floor;
        self.sweep_deadline.send_modify(|stored| {
            folded = stored.map_or(floor, |earlier| earlier.min(floor));
            *stored = Some(folded);
        });
        folded
    }

    /// The system's sweep deadline as it stands now, or `own` when none is
    /// held. A sweep in flight keeps a shutdown counted, so its own folded
    /// value is held until it completes.
    fn current_sweep_deadline(&self, own: Instant) -> Instant {
        self.sweep_deadline
            .borrow()
            .map_or(own, |held| held.min(own))
    }

    /// Stops `roots` one at a time through the sequential ladder, in the
    /// order given. The moment the global deadline cuts a root's ladder, that
    /// root and every root after it go to one concurrent force sweep instead,
    /// the cut root marked with whether its ladder had raised `Kill`.
    async fn stop_roots(
        &self,
        roots: &[RootSnapshot],
        per_actor_timeout: Duration,
        deadline: Instant,
    ) -> Vec<(ActorId, StopReport)> {
        let mut outcomes = Vec::with_capacity(roots.len());
        for (idx, root) in roots.iter().enumerate() {
            match self
                .stop_root_sequential(root, per_actor_timeout, deadline)
                .await
            {
                Ok(outcome) => outcomes.push((root.id.clone(), outcome)),
                Err(raised_kill) => {
                    let targets = roots[idx..]
                        .iter()
                        .enumerate()
                        .map(|(n, root)| SweepTarget {
                            id: root.id.clone(),
                            snapshot: root.clone(),
                            kill_raised: n == 0 && raised_kill,
                        })
                        .collect();
                    outcomes.extend(self.concurrent_force_sweep(targets, None).await);
                    break;
                }
            }
        }
        outcomes
    }

    /// Snapshots every currently-registered ROOT actor's id, registration
    /// sequence, a clone of its status receiver and its kill grace. The
    /// snapshot is independent of the registry entry from this point on: the
    /// `status_rx` clone stays valid even after the entry is removed by a
    /// `RegistryGuard` drop, and it names exactly this incarnation.
    fn snapshot_roots(&self) -> Vec<RootSnapshot> {
        self.by_id
            .iter()
            .filter(|e| e.parent.is_none())
            .map(|e| RootSnapshot {
                id: e.key().clone(),
                registration_seq: e.registration_seq,
                status_rx: e.status_rx.clone(),
                kill_grace: e.kill_grace,
                record: e.stop_lane.abort_record().clone(),
            })
            .collect()
    }

    /// Runs one root through the escalation ladder (`ParentRequest` -> `Kill`
    /// -> `abort()`), each wait bounded by whichever is tighter: the step's
    /// own bound, or the time left before the global deadline. Every step
    /// acts only on the incarnation `root` snapshotted: a later registration
    /// under the same id is never signalled or aborted. The verdict is
    /// `Killed` only once this ladder raised `Kill` on the incarnation; an
    /// incarnation that left the registry before the `Kill` step ends with
    /// the outcome it reaches by itself. Returns `Err(kill_raised)` the
    /// instant the global deadline is hit without having reached a terminal
    /// status, saying whether this ladder had raised `Kill` by then - the
    /// caller then hands this root (and every root after it) to the
    /// concurrent force sweep instead of continuing the sequential ladder.
    async fn stop_root_sequential(
        &self,
        root: &RootSnapshot,
        per_actor_timeout: Duration,
        deadline: Instant,
    ) -> Result<StopReport, bool> {
        let id = &root.id;
        let mut status_rx = root.status_rx.clone();

        // Step 1: ParentRequest (vetoable via pre_stop).
        #[cfg(test)]
        self.run_ladder_hook(LadderPoint::Stop, id);
        self.send_stop(id, &status_rx, StopReason::ParentRequest);
        if wait_terminal(&mut status_rx, time_left(deadline).min(per_actor_timeout)).await {
            return Ok(terminal_report(&status_rx, StopOutcome::Graceful));
        }
        if Instant::now() >= deadline {
            return Err(false);
        }

        // Step 2: Kill (unvetoable, but still only observed cooperatively -
        // an actor stuck inside a callback never reaches the select! that
        // would see it). The system schedules the abort when it raises the
        // Kill, so a walk cut short from here on still ends in one.
        #[cfg(test)]
        self.run_ladder_hook(LadderPoint::Kill, id);
        let kill_raised = self.raise_kill(id, &status_rx);
        let requested = if kill_raised {
            StopOutcome::Killed
        } else {
            StopOutcome::Graceful
        };
        if wait_terminal(&mut status_rx, time_left(deadline).min(root.kill_grace)).await {
            return Ok(terminal_report(&status_rx, requested));
        }
        if Instant::now() >= deadline {
            return Err(kill_raised);
        }

        // Step 3: abort() - the last resort for an actor that never yielded
        // back to the run loop (so Kill was never even observed).
        #[cfg(test)]
        self.run_ladder_hook(LadderPoint::Abort, id);
        self.abort_by_id(id, &status_rx);
        if wait_terminal(&mut status_rx, time_left(deadline).min(ABORT_SETTLE)).await {
            return Ok(terminal_report(&status_rx, requested));
        }

        // Nothing left to hand off to: the concurrent sweep would do exactly
        // what steps 2-3 just did. A genuinely non-yielding task.
        Ok(unresponsive())
    }

    /// Force-stops a batch CONCURRENTLY: `Kill` -> grace -> `abort()` ->
    /// grace, skipping the vetoable `ParentRequest` step entirely. Used once
    /// the global deadline is breached (remaining roots) and for the
    /// leftovers the final sweep still finds running at the global deadline:
    /// in both cases the time for a graceful stop has run out. Every id is
    /// reported exactly once, and no target's wait holds up another's.
    ///
    /// A target is looked up as the incarnation its snapshot kept. One the
    /// registry does not hold as that incarnation is waited on through the
    /// status receiver its snapshot kept, bounded as a found target is (its
    /// kill grace, then the wait after an abort), because the runtime frees
    /// an actor's registry entry before it publishes `Stopped`. A root's
    /// terminal end goes through the one terminal classifier: `Stopped`
    /// holds as `Killed` when this shutdown had raised `Kill` on it and as
    /// `Graceful` otherwise, a sender closed without `Stopped` is `Aborted`.
    /// A leftover's (`sweep_deadline` is the sweep's deadline) goes through
    /// the leftover classifier, [`leftover_report`], against the system's
    /// deadline read when its force ends. One still open after the bound is
    /// `Unresponsive`. A force task that fails (it panics or is cancelled)
    /// still has its target reported, from the target's plane: as its
    /// classifier reads it once the plane is terminal, `Unresponsive` while
    /// it still runs.
    async fn concurrent_force_sweep(
        &self,
        targets: Vec<SweepTarget>,
        sweep_deadline: Option<Instant>,
    ) -> Vec<(ActorId, StopReport)> {
        let mut set = JoinSet::new();
        let count = targets.len();
        // Each task's target, its plane, the outcome its classifier asks for
        // and whether this shutdown raised `Kill` on it, fixed here before
        // the task is spawned.
        let mut by_task: HashMap<tokio::task::Id, (ActorId, StatusRx, StopOutcome, bool)> =
            HashMap::new();
        for target in targets {
            let SweepTarget {
                id,
                snapshot,
                kill_raised,
            } = target;
            #[cfg(test)]
            self.run_ladder_hook(LadderPoint::SweepLookup, &id);
            let found = self
                .by_id
                .get(&id)
                .filter(|e| e.status_rx.same_channel(&snapshot.status_rx))
                .map(|e| (e.status_rx.clone(), e.stop_lane.clone(), e.kill_grace));
            let Some((status_rx, lane, grace)) = found else {
                // The registry does not hold the incarnation the sweep is after.
                let requested = if kill_raised {
                    StopOutcome::Killed
                } else {
                    StopOutcome::Graceful
                };
                let rx = snapshot.status_rx.clone();
                let record = snapshot.record.clone();
                let grace = snapshot.kill_grace;
                let task =
                    set.spawn(async move { force_stop(rx, None, record, grace, requested).await });
                by_task.insert(task.id(), (id, snapshot.status_rx, requested, kill_raised));
                continue;
            };
            #[cfg(test)]
            self.run_after_kill_lookup();
            // A lane that refuses the `Kill` belongs to an incarnation
            // already past its last read of it: nothing is armed, it ends
            // with its own reason, and the verdict is `Killed` only if this
            // shutdown raised `Kill` on it earlier. The wait is the
            // incarnation's own grace, read with the lookup, so it holds
            // after the entry is gone.
            let raised_kill = raise_kill_declared(
                Some(self),
                KillKey::Id(&id),
                &status_rx,
                &lane,
                None,
                None,
                || {},
                None,
            );
            let took = raised_kill.result != RaiseResult::Refused;
            let raised = took || kill_raised;
            let requested = if raised {
                StopOutcome::Killed
            } else {
                StopOutcome::Graceful
            };
            let record = lane.abort_record().clone();
            let abort = if took {
                raised_kill.abort
            } else {
                self.kill_target_for(&id, &status_rx)
                    .and_then(|(abort, _, _)| abort)
            };
            let rx = status_rx.clone();
            let task =
                set.spawn(async move { force_stop(rx, abort, record, grace, requested).await });
            by_task.insert(task.id(), (id, status_rx, requested, raised));
        }

        let mut results = Vec::with_capacity(count);
        while let Some(joined) = set.join_next_with_id().await {
            let (task, report) = match joined {
                Ok((task, report)) => (task, Some(report)),
                Err(err) => (err.id(), None),
            };
            let Some((id, rx, requested, raised)) = by_task.remove(&task) else {
                continue;
            };
            let terminal = match &report {
                Some(report) => report.outcome != StopOutcome::Unresponsive,
                None => read_status(&rx) == ActorStatus::Stopped,
            };
            let report = if !terminal {
                report.unwrap_or_else(unresponsive)
            } else if let Some(deadline) = sweep_deadline {
                leftover_report(&rx, raised, self.current_sweep_deadline(deadline))
            } else {
                report.unwrap_or_else(|| terminal_report(&rx, requested))
            };
            results.push((id, report));
        }
        results
    }

    /// Clones the lane of the incarnation whose status plane is `status_rx`
    /// out of the map guard and drops the guard before raising it. A missing
    /// entry (the actor already died on its own), or one that is a later
    /// incarnation under the same id, is a silent no-op: the caller's
    /// subsequent status wait finds the incarnation already terminal.
    fn send_stop(&self, id: &ActorId, status_rx: &StatusRx, reason: StopReason) {
        let lane = self
            .by_id
            .get(id)
            .filter(|e| e.status_rx.same_channel(status_rx))
            .map(|e| e.stop_lane.clone());
        if let Some(lane) = lane {
            raise_noted(
                Some(self),
                &lane,
                reason,
                crate::telemetry::StopOrigin::Requested,
            );
        }
    }

    /// Raises `Kill` on the incarnation whose status plane is `status_rx` and
    /// schedules the abort of its task once its kill grace has passed.
    /// Returns whether the `Kill` was raised: it is not when the registry does
    /// not hold that incarnation under `id` (it left, or a later incarnation
    /// holds the id) or its lane refuses the request.
    fn raise_kill(&self, id: &ActorId, status_rx: &StatusRx) -> bool {
        let Some(lane) = self
            .by_id
            .get(id)
            .filter(|e| e.status_rx.same_channel(status_rx))
            .map(|e| e.stop_lane.clone())
        else {
            return false;
        };
        #[cfg(test)]
        self.run_after_kill_lookup();
        raise_kill_declared(
            Some(self),
            KillKey::Id(id),
            status_rx,
            &lane,
            None,
            None,
            || {},
            None,
        )
        .result
            != RaiseResult::Refused
    }

    /// Aborts the task of the incarnation whose status plane is `status_rx`.
    fn abort_by_id(&self, id: &ActorId, status_rx: &StatusRx) {
        let target = self
            .by_id
            .get(id)
            .filter(|e| e.status_rx.same_channel(status_rx))
            .map(|e| (e.abort.clone(), e.stop_lane.abort_record().clone()));
        if let Some((Some(abort), record)) = target {
            record.abort(&abort);
        }
    }
}

// ---------------------------------------------------------------------------
// Stuck-turn watchdog: threshold encoding and the sweep task
// ---------------------------------------------------------------------------

/// Encodes a threshold for storage in `turn_warning_threshold_nanos`: `0`
/// means off. `Some(Duration::ZERO)` is treated the same as `None` (see
/// [`ActorSystem::set_turn_warning_threshold`]), and a `Duration` too large
/// to fit in a `u64` nanosecond count saturates to `u64::MAX` instead of
/// overflowing.
fn threshold_to_nanos(threshold: Option<Duration>) -> u64 {
    match threshold {
        None => 0,
        Some(d) if d.is_zero() => 0,
        Some(d) => u64::try_from(d.as_nanos()).unwrap_or(u64::MAX),
    }
}

/// Decodes a `turn_warning_threshold_nanos` value back into the `Option`
/// surface: `0` is `None`.
fn nanos_to_threshold(nanos: u64) -> Option<Duration> {
    (nanos != 0).then(|| Duration::from_nanos(nanos))
}

/// Clears a sweep task's shared spawn latch back to `false` when dropped:
/// whether the task it travels with returns normally, is aborted mid-flight,
/// or is dropped without ever being polled at all, because the runtime it
/// was spawned on shuts down first. Without this, a sweep task that never
/// clears the latch would leave `sweep_spawned` stuck at `true` forever, and
/// no later runtime could ever spawn a replacement for it.
struct SweepLatchGuard(Arc<AtomicBool>);

impl Drop for SweepLatchGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// How long the turn-watchdog sweep sleeps between checks for a given turn
/// warning threshold: a quarter of the threshold, never less than 50 ms and
/// never more than 1 s. A zero threshold gives 50 ms and `Duration::MAX`
/// gives 1 s; the division cannot overflow.
fn sweep_tick(threshold: Duration) -> Duration {
    (threshold / 4).clamp(Duration::from_millis(50), Duration::from_secs(1))
}

/// An actor whose turn ran past a threshold, as the sweep saw it. It holds
/// the actor's record itself, not the record's address: the record names the
/// actor in the telemetry table for as long as it lives, so an actor that
/// ends while the sweep writes the lines before its own is still named by its
/// own identity and never by a new actor placed at the same address.
struct StuckTurn {
    id: ActorId,
    seq: u64,
    token: u64,
    elapsed: Duration,
    record: crate::actor::subtree::AbortRecord,
}

impl StuckTurn {
    /// The address that names the record in the telemetry table.
    #[cfg(test)]
    fn key(&self) -> usize {
        self.record.key()
    }

    /// The identity the telemetry table holds for this actor's record.
    #[cfg(feature = "tracing")]
    fn identity(&self, mode: &crate::telemetry::Mode) -> Option<Arc<crate::telemetry::Identity>> {
        mode.find(self.record.key())
    }
}

/// The actors whose current turn has run longer than `threshold`.
fn stuck_turns(system: &ActorSystem, threshold: Duration) -> Vec<StuckTurn> {
    system
        .by_id
        .iter()
        .filter_map(|e| {
            let (token, elapsed) = e.turn_clock.snapshot()?;
            (elapsed > threshold).then(|| StuckTurn {
                id: e.key().clone(),
                seq: e.registration_seq,
                token,
                elapsed,
                record: e.stop_lane.abort_record().clone(),
            })
        })
        .collect()
}

/// Background task, spawned by [`ActorSystem::ensure_sweep_spawned_with`]
/// whenever none is currently alive for this system and something asks for
/// it, that looks at the actors now and then: it reports a turn that has run
/// past the smaller of the monitor's `long_schedule` and the turn warning
/// threshold, whichever of them is set, reports a mailbox that crossed the
/// monitor's lengths, and with the `tracing` feature logs actors over the
/// turn warning threshold. Holds its system only weakly and upgrades it for
/// the length of one look, and exits the moment that upgrade comes back
/// empty - so this task never keeps an otherwise-unreferenced system alive,
/// never sweeps a later system that claimed the name, and never needs a
/// separate shutdown signal. It also exits after two looks in a row that
/// found nothing asking for it. The caller spawns this alongside a
/// [`SweepLatchGuard`] built from the owning system's `sweep_spawned`, so
/// that latch clears on the way out by any exit path, including this task
/// being dropped before it is ever polled, or being dropped outright by its
/// runtime shutting down.
async fn turn_watchdog_sweep(system: Weak<ActorSystem>) {
    let mut state = crate::telemetry::monitor::SweepState::default();
    let mut idle_looks = 0u8;
    loop {
        let Some(live) = system.upgrade() else {
            return;
        };
        if live.sweep_wanted() {
            idle_looks = 0;
        } else {
            idle_looks += 1;
            if idle_looks >= 2 {
                return;
            }
        }
        // The look has no `.await` in it: never hold a `DashMap` guard across
        // one.
        let tick = live.sweep_look(&mut state);
        drop(live);
        tokio::time::sleep(tick).await;
    }
}

impl ActorSystem {
    /// One look of the sweep; returns how long to wait before the next.
    ///
    /// The smallest nonzero threshold among the turn warning threshold and
    /// the monitor's `long_schedule` is the one a turn is reported at (once
    /// per turn), and a quarter of it sets the wait; a monitor of mailboxes
    /// alone counts as a second. Events go to the pump; reports for the
    /// system monitor go to its inbox; no handler and no map runs here.
    fn sweep_look(&self, state: &mut crate::telemetry::monitor::SweepState) -> Duration {
        use crate::telemetry::EventKind;

        let mode = self.telemetry_mode();
        let watchdog = self.turn_warning_threshold_nanos.load(Ordering::Relaxed);
        let long_schedule = mode.monitor_long_schedule_nanos();
        let smallest = [watchdog, long_schedule]
            .into_iter()
            .filter(|nanos| *nanos != 0)
            .min()
            .map(Duration::from_nanos);
        let tick = sweep_tick(smallest.unwrap_or(Duration::from_secs(1)));
        state.pass += 1;
        let inbox = self.monitor_inbox_for(state);

        let turns_inbox = inbox.as_ref().filter(|_| long_schedule != 0);
        let report =
            smallest.is_some() && (mode.armed(EventKind::TurnStuck) || turns_inbox.is_some());
        let warn = cfg!(feature = "tracing") && watchdog != 0;
        match smallest {
            Some(threshold) if report || warn => {
                let turns = stuck_turns(self, threshold);
                if report {
                    self.report_stuck_turns(&turns, threshold, state, turns_inbox.map(|i| &**i));
                } else {
                    state.stuck.clear();
                }
                #[cfg(feature = "tracing")]
                if warn {
                    self.warn_stuck_turns(&turns, Duration::from_nanos(watchdog), state);
                }
            }
            _ => state.stuck.clear(),
        }

        match crate::telemetry::monitor::unpack_queue(mode.monitor_queue()) {
            Some((enable, disable)) => {
                self.report_backlogs(enable, disable, state, inbox.as_deref());
            }
            _ => {
                state.alarms.clear();
                state.leaving.clear();
            }
        }
        tick
    }

    /// The system monitor's inbox for this look, after the helper's answers
    /// since the last look are read into `state`. A monitor that is not the
    /// one the tracked states were told to is owed none of them: they read as
    /// owed nothing, the offers and short reports still out are forgotten,
    /// and the look is that monitor's first.
    fn monitor_inbox_for(
        &self,
        state: &mut crate::telemetry::monitor::SweepState,
    ) -> Option<Arc<crate::telemetry::monitor::MonitorInbox>> {
        use crate::telemetry::monitor::{Offer, Told};

        let current = crate::telemetry::monitor::inbox(self.telemetry_mode());
        let id = current.as_ref().map(|(id, _)| *id);
        state.first_look = state.monitor != id;
        if state.first_look {
            state.monitor = id;
            state.offered.clear();
            state.leaving.clear();
            for stuck in state.stuck.values_mut() {
                stuck.told = Told::Unowed;
            }
            for alarm in state.alarms.values_mut() {
                alarm.told = Told::Unowed;
            }
        }
        let (_, inbox) = current?;
        for (tag, landed) in inbox.take_outcomes() {
            let Some(offer) = state.offered.remove(&tag) else {
                continue;
            };
            let answer = if landed { Told::Landed } else { Told::Refused };
            // An answer counts only for the offer the state still waits on:
            // a state that ended or moved on since has another offer or none.
            match offer {
                Offer::Turn(seq) => {
                    if let Some(stuck) = state.stuck.get_mut(&seq) {
                        if stuck.told == Told::Pending(tag) {
                            stuck.told = answer;
                        }
                    }
                }
                Offer::Entered(seq) => {
                    if let Some(alarm) = state.alarms.get_mut(&seq) {
                        if alarm.told == Told::Pending(tag) {
                            alarm.told = answer;
                        }
                    }
                    // A short report held behind this long one goes out once
                    // the long one landed, and never when it was refused.
                    if state
                        .leaving
                        .get(&seq)
                        .is_some_and(|l| l.after == Some(tag))
                    {
                        if landed {
                            if let Some(leaving) = state.leaving.get_mut(&seq) {
                                leaving.after = None;
                            }
                        } else {
                            state.leaving.remove(&seq);
                        }
                    }
                }
                Offer::Left(seq) => {
                    if state
                        .leaving
                        .get(&seq)
                        .is_some_and(|l| l.told == Told::Pending(tag))
                    {
                        if landed {
                            state.leaving.remove(&seq);
                        } else if let Some(leaving) = state.leaving.get_mut(&seq) {
                            leaving.told = Told::Refused;
                        }
                    }
                }
            }
        }
        inbox.prompt_missed();
        Some(inbox)
    }

    /// Offers `event` to the monitor as a new offer for `what`; the state's
    /// `Told` after the offer.
    fn offer_to_monitor(
        state: &mut crate::telemetry::monitor::SweepState,
        inbox: &crate::telemetry::monitor::MonitorInbox,
        what: crate::telemetry::monitor::Offer,
        event: TelemetryEvent<'static>,
    ) -> crate::telemetry::monitor::Told {
        use crate::telemetry::monitor::Told;
        state.next_tag += 1;
        let tag = state.next_tag;
        if inbox.offer(event, tag) {
            state.offered.insert(tag, what);
            Told::Pending(tag)
        } else {
            Told::Refused
        }
    }

    /// Reports each turn in `turns` that was not reported yet, once, to the
    /// sinks that take the kind; offers it to the monitor's `inbox` until the
    /// monitor's mailbox takes it while the turn lasts; and forgets the turns
    /// that are over.
    fn report_stuck_turns(
        &self,
        turns: &[StuckTurn],
        threshold: Duration,
        state: &mut crate::telemetry::monitor::SweepState,
        inbox: Option<&crate::telemetry::monitor::MonitorInbox>,
    ) {
        use crate::telemetry::monitor::{Offer, Stuck, Told};
        use crate::telemetry::EventKind;

        let mode = self.telemetry_mode();
        let pass = state.pass;
        for turn in turns {
            let known = state
                .stuck
                .get(&turn.seq)
                .filter(|stuck| stuck.token == turn.token);
            let new = known.is_none();
            let told = known.map_or(Told::Unoffered, |stuck| stuck.told);
            let to_sinks = new && mode.armed(EventKind::TurnStuck);
            let to_monitor = inbox.is_some() && matches!(told, Told::Unoffered | Told::Refused);
            let mut told = if inbox.is_some() { told } else { Told::Unowed };
            if to_sinks || to_monitor {
                match crate::telemetry::identity_for(mode, &turn.record, Some(self), Some(&turn.id))
                {
                    Some(identity) => {
                        let event = || TelemetryEvent::TurnStuck {
                            actor: identity.actor().clone(),
                            elapsed: turn.elapsed,
                            threshold,
                        };
                        if to_sinks {
                            mode.push_owned(&identity, EventKind::TurnStuck, event);
                        }
                        if let Some(inbox) = inbox.filter(|_| to_monitor) {
                            told = if inbox.is_monitor(identity.actor()) {
                                Told::Unowed
                            } else {
                                Self::offer_to_monitor(state, inbox, Offer::Turn(turn.seq), event())
                            };
                        }
                    }
                    None => {
                        // The registry has let go of the actor and nothing
                        // recorded it, so no report can name it: counted with
                        // the other drops.
                        mode.pump.count_dropped();
                        told = Told::Unowed;
                    }
                }
            }
            state.stuck.insert(
                turn.seq,
                Stuck {
                    token: turn.token,
                    seen: pass,
                    told,
                },
            );
        }
        state.stuck.retain(|_, stuck| stuck.seen == pass);
    }

    /// Logs each turn in `turns` over `threshold` that was not logged yet.
    #[cfg(feature = "tracing")]
    fn warn_stuck_turns(
        &self,
        turns: &[StuckTurn],
        threshold: Duration,
        state: &mut crate::telemetry::monitor::SweepState,
    ) {
        let mode = self.telemetry_mode();
        let system_name = self.name();
        let mut still_stuck: HashSet<ActorId> = HashSet::new();
        for turn in turns.iter().filter(|turn| turn.elapsed > threshold) {
            let (id, token, elapsed) = (&turn.id, turn.token, turn.elapsed);
            still_stuck.insert(id.clone());
            if state.warned.get(id) != Some(&token) {
                let elapsed_ms = elapsed.as_millis() as u64;
                let threshold_ms = threshold.as_millis() as u64;
                let fields = crate::telemetry::LogFields::of(turn.identity(mode), system_name);
                caught_log!(warn,
                    actor_id = %id,
                    actor_name = fields.name(),
                    incarnation = fields.incarnation(),
                    system = fields.system(),
                    elapsed_ms,
                    threshold_ms,
                    "actor turn exceeds the turn warning threshold"
                );
                state.warned.insert(id.clone(), token);
            }
        }
        state.warned.retain(|id, _| still_stuck.contains(id));
    }

    /// Reports each mailbox that reached `enable` and each reported mailbox
    /// that fell to `disable` or whose actor is gone, once per edge, to the
    /// sinks that take the kind. The monitor's `inbox` is offered the same
    /// edges until its mailbox takes each while it lasts; a short report is
    /// offered only for a long report the monitor took.
    fn report_backlogs(
        &self,
        enable: usize,
        disable: usize,
        state: &mut crate::telemetry::monitor::SweepState,
        inbox: Option<&crate::telemetry::monitor::MonitorInbox>,
    ) {
        use crate::telemetry::monitor::{backlog_edge, Alarm, Leaving, Offer, Told};
        use crate::telemetry::EventKind;

        let mode = self.telemetry_mode();
        let to_sinks = mode.armed(EventKind::MailboxBacklog);
        let pass = state.pass;
        // Collected under the registry guards; the identities are resolved
        // and the events pushed after the last guard is gone.
        let mut entered: Vec<(u64, ActorId, AbortRecord, usize, usize)> = Vec::new();
        let mut left: Vec<(u64, Alarm, usize)> = Vec::new();
        let mut long: Vec<(u64, usize)> = Vec::new();
        for entry in &self.by_id {
            let seq = entry.registration_seq;
            let alarmed = state.alarms.contains_key(&seq);
            let view = entry.handle.mail_view();
            let Some(edge) = backlog_edge(alarmed, view.mailbox_len, enable, disable) else {
                if let Some(alarm) = state.alarms.get_mut(&seq) {
                    alarm.seen = pass;
                    long.push((seq, view.mailbox_len));
                }
                continue;
            };
            if entry.status_rx.borrow().reason.is_some() {
                // An ended actor is gone as far as its mailbox is concerned.
                continue;
            }
            if edge {
                entered.push((
                    seq,
                    entry.key().clone(),
                    entry.stop_lane.abort_record().clone(),
                    view.mailbox_len,
                    view.mailbox_capacity,
                ));
            } else if let Some(alarm) = state.alarms.remove(&seq) {
                left.push((seq, alarm, view.mailbox_len));
            }
        }
        // A reported mailbox the walk did not see alive is reported short once.
        let gone: Vec<u64> = state
            .alarms
            .iter()
            .filter(|(_, alarm)| alarm.seen != pass)
            .map(|(seq, _)| *seq)
            .collect();
        for seq in gone {
            if let Some(alarm) = state.alarms.remove(&seq) {
                left.push((seq, alarm, 0));
            }
        }
        for (seq, alarm, len) in left {
            let event = || TelemetryEvent::MailboxBacklog {
                actor: alarm.identity.actor().clone(),
                entered: false,
                len,
                capacity: alarm.capacity,
            };
            if to_sinks {
                mode.push_owned(&alarm.identity, EventKind::MailboxBacklog, event);
            }
            // The monitor is owed the short report only for a long one it
            // took, or one whose answer is still out.
            let after = match alarm.told {
                Told::Landed => None,
                Told::Pending(tag) => Some(tag),
                Told::Unoffered | Told::Refused | Told::Unowed => continue,
            };
            if inbox.is_none() {
                continue;
            }
            state.leaving.insert(
                seq,
                Leaving {
                    identity: alarm.identity.clone(),
                    len,
                    capacity: alarm.capacity,
                    after,
                    told: Told::Unoffered,
                },
            );
        }
        for (seq, id, record, len, capacity) in entered {
            let Some(identity) =
                crate::telemetry::identity_for(mode, &record, Some(self), Some(&id))
            else {
                // The registry has let go of the actor and nothing recorded
                // it, so no report can name it: counted with the other drops.
                mode.pump.count_dropped();
                continue;
            };
            if to_sinks {
                mode.push_owned(&identity, EventKind::MailboxBacklog, || {
                    TelemetryEvent::MailboxBacklog {
                        actor: identity.actor().clone(),
                        entered: true,
                        len,
                        capacity,
                    }
                });
            }
            // A mailbox long again ends the short state the monitor was
            // still owed.
            state.leaving.remove(&seq);
            let told = match inbox {
                // A mailbox long at the monitor's first look stood before it,
                // and the monitor's own mailbox is never reported to it.
                Some(inbox) if !state.first_look && !inbox.is_monitor(identity.actor()) => {
                    Told::Unoffered
                }
                _ => Told::Unowed,
            };
            state.alarms.insert(
                seq,
                Alarm {
                    identity,
                    capacity,
                    seen: pass,
                    told,
                },
            );
            long.push((seq, len));
        }
        let Some(inbox) = inbox else {
            return;
        };
        // Offer each long mailbox the monitor has not taken yet, with its
        // length now, and each short report owed and not held back.
        for (seq, len) in long {
            let Some(alarm) = state.alarms.get(&seq) else {
                continue;
            };
            if !matches!(alarm.told, Told::Unoffered | Told::Refused) {
                continue;
            }
            let event = TelemetryEvent::MailboxBacklog {
                actor: alarm.identity.actor().clone(),
                entered: true,
                len,
                capacity: alarm.capacity,
            };
            let told = Self::offer_to_monitor(state, inbox, Offer::Entered(seq), event);
            if let Some(alarm) = state.alarms.get_mut(&seq) {
                alarm.told = told;
            }
        }
        let owed: Vec<u64> = state
            .leaving
            .iter()
            .filter(|(_, l)| l.after.is_none() && matches!(l.told, Told::Unoffered | Told::Refused))
            .map(|(seq, _)| *seq)
            .collect();
        for seq in owed {
            let Some(leaving) = state.leaving.get(&seq) else {
                continue;
            };
            let event = TelemetryEvent::MailboxBacklog {
                actor: leaving.identity.actor().clone(),
                entered: false,
                len: leaving.len,
                capacity: leaving.capacity,
            };
            let told = Self::offer_to_monitor(state, inbox, Offer::Left(seq), event);
            if let Some(leaving) = state.leaving.get_mut(&seq) {
                leaving.told = told;
            }
        }
    }
}

/// The points at which a test hook runs: before each step of a root's
/// sequential shutdown ladder, before the force sweep looks a target up, and
/// at seven points of `ActorHandle::stop_within`: when its stop task starts,
/// before the task raises its `Kill`, between the step cell's store and the
/// raise, after it raised it, after it answered, inside the task's loss
/// action once the loss has been handled, and in the caller when it finds
/// the task's answer closed.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LadderPoint {
    Stop,
    Kill,
    Abort,
    SweepLookup,
    StopBeforeRaise,
    StopHelperStart,
    StopWithinKill,
    StopKillStored,
    StopAfterKill,
    StopHelperLost,
    StopAnswered,
    StopClosedSeen,
    StopRaised,
    StopDecided,
}

#[cfg(test)]
type LadderHook = Arc<dyn Fn(LadderPoint, &ActorId) + Send + Sync>;

/// How many runtimes without the time driver a system remembers before it
/// starts its memo over.
const TIMERLESS_MEMO_LIMIT: usize = 64;

/// The channel a shutdown walk run by a helper task reports on. The report is
/// sent once, contained; a walk that ends without one (it panicked, or its
/// runtime ended) drops the sender, which wakes the shutdown's caller, whose
/// waker belongs to the caller's task. That drop can run while the walk
/// unwinds, so a panic of the waker is contained there too.
struct WalkReport(Option<oneshot::Sender<ShutdownReport>>);

impl WalkReport {
    fn send(mut self, report: ShutdownReport) {
        if let Some(tx) = self.0.take() {
            let _ = send_contained(tx, report);
        }
    }
}

impl Drop for WalkReport {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            contain_waker_panic("shutdown report", None, move || drop(tx));
        }
    }
}

/// What the move to `Defunct` takes out of a system: the process map's strong
/// reference to it and its reaper feeds.
type Released = (
    Option<Arc<ActorSystem>>,
    HashMap<tokio::runtime::Id, ReaperSlot>,
);

/// Counts one shutdown in flight (see `ActorSystem::registration_gate`):
/// entered with the phase change out of `Active`, and left when the shutdown
/// ends, completed or cut short. A completed one records that the system has
/// completed a shutdown since it was last `Active`; the shutdown that lowers
/// the count to zero stores `Defunct` when that is recorded, whichever order
/// the calls ended in. Registrations are refused from the first entry after
/// the system was last `Active` and every walk snapshots after its own entry,
/// so a completed walk has stopped every root registered before that. Only
/// cut-short shutdowns leave the system refusing registrations until a
/// shutdown of the system has completed (an earlier one counts).
struct ShutdownInFlight<S: std::ops::Deref<Target = ActorSystem>> {
    system: S,
    completed: bool,
}

impl<S: std::ops::Deref<Target = ActorSystem>> ShutdownInFlight<S> {
    fn enter(system: S) -> Self {
        {
            let mut in_flight = system
                .registration_gate
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *in_flight += 1;
            system
                .phase
                .store(SystemPhase::ShuttingDown.as_u8(), Ordering::Release);
        }
        Self {
            system,
            completed: false,
        }
    }
}

impl<S: std::ops::Deref<Target = ActorSystem>> Drop for ShutdownInFlight<S> {
    fn drop(&mut self) {
        // What the move to `Defunct` lets go of, dropped after the gate is
        // released: the map's strong reference to the system and the reaper
        // feeds (each reaper fires what it holds, then ends once the last
        // clone of its feed is gone; the next feed starts a new one).
        let mut released: Option<Released> = None;
        {
            let mut in_flight = self
                .system
                .registration_gate
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *in_flight -= 1;
            if self.completed {
                self.system
                    .completed_since_active
                    .store(true, Ordering::Relaxed);
            }
            if *in_flight == 0 && self.system.completed_since_active.load(Ordering::Relaxed) {
                // No sweep of this system is in flight any more: the deadline
                // it held for them goes with the move to `Defunct`, under the
                // same lock, so a shutdown after a reactivation folds only its
                // own.
                self.system.sweep_deadline.send_replace(None);
                self.system
                    .phase
                    .store(SystemPhase::Defunct.as_u8(), Ordering::Release);
                // The map's hold goes in the same section, so the slot is
                // strong exactly until `Defunct` is stored.
                let slot = self.system.demote_slot();
                #[cfg(test)]
                {
                    let hook = self
                        .system
                        .after_demotion
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                let reapers = std::mem::take(
                    &mut *self
                        .system
                        .reaper
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
                released = Some((slot, reapers));
            }
        }
        drop(released);
    }
}

/// One target of the concurrent force sweep: its id, the snapshot of the
/// incarnation it is (a root's from the start of the shutdown, a non-root
/// leftover's from the listing that found it), and whether this shutdown had
/// already raised `Kill` on it.
struct SweepTarget {
    id: ActorId,
    snapshot: RootSnapshot,
    kill_raised: bool,
}

/// Snapshot of a root actor taken at the start of shutdown: independent of
/// the registry entry (the `status_rx` clone stays valid even after
/// `RegistryGuard` removes the entry), and exact to one incarnation (its
/// status channel is that incarnation's own).
#[derive(Clone)]
struct RootSnapshot {
    id: ActorId,
    registration_seq: u64,
    status_rx: StatusRx,
    kill_grace: Duration,
    /// The incarnation's abort record: a leftover whose registry entry is
    /// already freed is judged by the abort the reaper issues against it.
    record: AbortRecord,
}

/// Time remaining until `deadline`, floored at zero.
fn time_left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Reads an actor's status from its status channel, treating a closed
/// sender as terminal: an aborted task never writes a final status itself,
/// so the sender closing is itself the terminal signal, the same one
/// [`wait_terminal`] already treats as terminal.
pub(crate) fn read_status(rx: &StatusRx) -> ActorStatus {
    if rx.has_changed().is_err() {
        ActorStatus::Stopped
    } else {
        rx.status()
    }
}

/// The reason a terminal plane carries: the one written, or `Kill` for a
/// plane that closed without one. Meaningful only once the plane is
/// terminal.
pub(crate) fn terminal_reason(rx: &StatusRx) -> StopReason {
    rx.reason_or_kill()
}

/// Waits, without a bound, until the plane is terminal and returns its
/// reason.
pub(crate) async fn wait_reason(rx: &mut StatusRx) -> StopReason {
    loop {
        if rx.status() == ActorStatus::Stopped {
            return rx.reason_or_kill();
        }
        if rx.changed().await.is_err() {
            return rx.reason_or_kill();
        }
    }
}

/// Waits, without a bound, until the plane is closed: every sender of it is
/// gone. An actor's senders are held by its task, its registry guard and its
/// timer and stream forwarders, so the plane closes once the actor's task has
/// dropped everything it owned and let go of the plane. That comes after
/// `Stopped` is published and after whatever runs between the publication and
/// the task's return (the end log line, with the `tracing` feature), and on
/// an abort with the task's drop. A version the plane carries does not end
/// the wait.
pub(crate) async fn wait_closed(rx: &mut StatusRx) {
    loop {
        if rx.has_changed().is_err() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// The key a monitor target is looked up by.
pub(crate) enum MonitorKey<'a> {
    /// An actor id.
    Id(&'a ActorId),
    /// A registered name.
    Name(&'a str),
}

/// What a monitor key resolved to. `status_rx` is `None` when nothing is
/// registered under the key, and then there is no run to name.
#[derive(Debug)]
pub(crate) struct ResolvedTarget {
    pub(crate) actor_id: ActorId,
    pub(crate) incarnation: Option<Incarnation>,
    pub(crate) status_rx: Option<StatusRx>,
    /// The target's stop lane, from the registry entry or the handle that
    /// named it: `None` exactly when `status_rx` is.
    pub(crate) lane: Option<StopLane>,
}

/// Waits, without a bound, until the plane carries the actor's final reason
/// (written at its lane seal; for a task that never reaches its seal, by its
/// abort guard once its name is free, or by its context when it is dropped
/// before its first poll) or closes, and returns that reason.
pub(crate) async fn wait_final_reason(rx: &mut StatusRx) -> StopReason {
    loop {
        let reason = rx.borrow().reason.clone();
        if let Some(reason) = reason {
            return reason;
        }
        if rx.status() == ActorStatus::Stopped {
            return rx.reason_or_kill();
        }
        if rx.changed().await.is_err() {
            return rx.reason_or_kill();
        }
    }
}

/// A final sweep's wait on one leftover: until its plane is terminal
/// (`Stopped` published, or closed) or the deadline passes. The deadline is
/// the earliest of `deadline` and every value the system's sweep deadline
/// takes while the wait runs, so a sweep that starts later with an earlier
/// deadline shortens it. Returns whether the plane was found terminal.
async fn wait_end_by_the_system_deadline(
    rx: &mut StatusRx,
    system: &mut watch::Receiver<Option<Instant>>,
    deadline: Instant,
) -> bool {
    let mut deadline = deadline;
    let mut system_open = true;
    loop {
        if let Some(earlier) = *system.borrow_and_update() {
            deadline = deadline.min(earlier);
        }
        if read_status(rx) == ActorStatus::Stopped {
            return true;
        }
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return true;
                }
            }
            changed = system.changed(), if system_open => {
                if changed.is_err() {
                    system_open = false;
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                return read_status(rx) == ActorStatus::Stopped;
            }
        }
    }
}

/// The one classifier of a final-sweep leftover whose plane is terminal
/// (`Stopped` published, or closed): a status other than `Stopped` (its task
/// ended without publishing it) reads `Aborted`; `Stopped` reads `Killed`
/// when this shutdown's force took a `Kill` on it (`raised`), or when its
/// end became final at or after `deadline` (or carries no instant), and
/// `Graceful` when it became final before. The reason is the one the plane
/// carries. Every terminal leftover goes through here, whichever path
/// produced its report, so one end reads one way whichever call reports it.
fn leftover_outcome(rx: &StatusRx, raised: bool, deadline: Instant) -> StopOutcome {
    let value = rx.borrow();
    if value.status != ActorStatus::Stopped {
        return StopOutcome::Aborted;
    }
    if raised || value.final_at.is_none_or(|at| at >= deadline) {
        StopOutcome::Killed
    } else {
        StopOutcome::Graceful
    }
}

/// The [`StopReport`] of a final-sweep leftover whose plane is terminal:
/// [`leftover_outcome`] and the reason the plane carries.
fn leftover_report(rx: &StatusRx, raised: bool, deadline: Instant) -> StopReport {
    StopReport {
        outcome: leftover_outcome(rx, raised, deadline),
        reason: Some(terminal_reason(rx)),
    }
}

/// The [`StopReport`] of a terminal wait: the outcome from
/// [`terminal_outcome`] and the reason the plane carries.
pub(crate) fn terminal_report(rx: &StatusRx, requested: StopOutcome) -> StopReport {
    StopReport {
        outcome: terminal_outcome(rx, requested),
        reason: Some(terminal_reason(rx)),
    }
}

/// The report of an actor that reached no terminal status within the
/// call's last bound.
fn unresponsive() -> StopReport {
    StopReport {
        outcome: StopOutcome::Unresponsive,
        reason: None,
    }
}

/// Waits, bounded by `bound`, for `status_rx` to reach a terminal state:
/// `ActorStatus::Stopped`, or the sender dropped without ever writing it (an
/// aborted task - terminal too, matching `ActorHandle::wait_stopped`).
/// Returns `true` if terminal was observed within `bound`.
pub(crate) async fn wait_terminal(status_rx: &mut StatusRx, bound: Duration) -> bool {
    tokio::time::timeout(bounded_wait(bound), async {
        while status_rx.status() != ActorStatus::Stopped {
            if status_rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await
    .is_ok()
}

/// The one classifier of a terminal wait. `requested` is the outcome the
/// call asked for (`Graceful` for a stop that raised a plain request, `Killed`
/// once a `Kill` was raised): it holds when the actor published `Stopped`
/// itself, and a status sender closed without that write means the task was
/// aborted, by the scheduled backstop or by the caller, and is `Aborted`.
/// Every terminal wait of `stop_within`, the shutdown ladder and the force
/// stops answers through here, so one condition gets one verdict on every
/// path, whichever of the deadlines fired first.
pub(crate) fn terminal_outcome(status_rx: &StatusRx, requested: StopOutcome) -> StopOutcome {
    if status_rx.status() == ActorStatus::Stopped {
        requested
    } else {
        StopOutcome::Aborted
    }
}

/// Waits until `deadline` for `status_rx` to reach a terminal state, as
/// [`wait_terminal`] does for a duration. Returns `true` if terminal was
/// observed first.
pub(crate) async fn wait_terminal_until(status_rx: &mut StatusRx, deadline: Instant) -> bool {
    tokio::time::timeout_at(deadline, async {
        while status_rx.borrow().status != ActorStatus::Stopped {
            if status_rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await
    .is_ok()
}

/// The step a bounded stop has reached, written by whoever takes its steps
/// and read by whoever has to classify the call without them: what a
/// `Stopped` the actor publishes reads as. A bounded stop starts at its
/// plain request. While raisers of its `Kill` are in flight the cell counts
/// them, and that count is the pending step: a reader that finds it waits.
/// Once any raiser's `Kill` is accepted the cell settles at `Killed` and
/// stays there. A raiser the lane refuses, or that unwinds before the raise,
/// gives back only its own share of the count, so the step is back at the
/// plain request only when no raiser of the call is left in flight.
///
/// The cell also holds the decision on the call's first request, for the
/// stop task a call spawns before it raises that request: the task takes no
/// step until the raise is decided. The caller decides right after the raise,
/// with nothing awaited in between.
#[derive(Debug, Default)]
pub(crate) struct StopStep(AtomicU16, AtomicU8);

/// What a stop task that is lost owes, as its decision reads at the loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LossOwes {
    /// The first request was accepted: the steps go to the reaper.
    Steps,
    /// The first request was refused, or is not decided yet and the caller
    /// takes over when it decides: nothing is handed over.
    Nothing,
}

impl StopStep {
    /// No raiser is in flight and no `Kill` was accepted. Any value between
    /// this and `KILLED` is the number of raisers in flight.
    const REQUESTED: u16 = 0;
    /// A `Kill` of the call was accepted; never left again.
    const KILLED: u16 = u16::MAX;

    /// The raise is not decided yet.
    const UNDECIDED: u8 = 0;
    /// The lane took the call's first request.
    const ACCEPTED: u8 = 1;
    /// The lane refused it: the call answers `Err` and owes no step.
    const REFUSED: u8 = 2;
    /// The task was lost before the decision; the caller owes its steps.
    const LOST_UNDECIDED: u8 = 3;

    /// The cell of a task spawned after the first request was accepted.
    pub(crate) fn accepted() -> Self {
        Self(
            AtomicU16::new(Self::REQUESTED),
            AtomicU8::new(Self::ACCEPTED),
        )
    }

    /// Decides the call's first request as refused: its task owes no step.
    /// Does nothing once the request is decided.
    pub(crate) fn decide_refused(&self) {
        let _ = self.1.compare_exchange(
            Self::UNDECIDED,
            Self::REFUSED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Decides the call's first request as accepted. Returns whether the
    /// stop task was lost before this decision: then it handed nothing over,
    /// and the caller hands the steps over itself.
    pub(crate) fn decide_accepted(&self) -> bool {
        self.1
            .compare_exchange(
                Self::UNDECIDED,
                Self::ACCEPTED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .err()
            == Some(Self::LOST_UNDECIDED)
    }

    /// Waits until the first request is decided, and returns whether it was
    /// accepted. The decider never awaits, so the wait ends as soon as the
    /// deciding thread runs, as the wait of [`requested`](Self::requested)
    /// for a step in progress does.
    pub(crate) async fn first_request_accepted(&self) -> bool {
        loop {
            match self.1.load(Ordering::SeqCst) {
                Self::UNDECIDED => tokio::task::yield_now().await,
                Self::REFUSED => return false,
                _ => return true,
            }
        }
    }

    /// Records the loss of the stop task and says what it owes: the steps
    /// when the request was accepted, nothing when it was refused, and
    /// nothing, with the loss recorded for the caller, when it is not decided.
    pub(crate) fn lose_before_the_last_wait(&self) -> LossOwes {
        match self.1.compare_exchange(
            Self::UNDECIDED,
            Self::LOST_UNDECIDED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => LossOwes::Nothing,
            Err(Self::ACCEPTED) => LossOwes::Steps,
            Err(_) => LossOwes::Nothing,
        }
    }

    /// Whether the call's first request was refused.
    #[cfg(test)]
    pub(crate) fn refused(&self) -> bool {
        self.1.load(Ordering::SeqCst) == Self::REFUSED
    }

    /// Raises `Kill` on `lane` as the step of the ladder. The raiser takes a
    /// share of the cell's count before the raise, so no one who sees the
    /// `Kill` take effect reads an earlier step, and a reader that finds the
    /// count waits for the step to settle. A raiser the lane accepts stores
    /// `Killed` when it leaves. A raiser the lane refuses, or that unwinds
    /// before the raise, gives back its share and nothing else, so it never
    /// settles the step while another raiser of the call is still in flight.
    /// A raiser that unwinds out of the raise itself stores `Killed`: the
    /// lane writes the request before its wakes run, so the `Kill` may
    /// already stand.
    ///
    /// A cell that reads `Killed` is never written again: an earlier raise of
    /// the same call was accepted, and a later raiser, accepted or refused,
    /// raises on the lane and leaves the cell alone, so no reader sees an
    /// earlier step for a moment. Every change of the count is a
    /// compare-exchange from the value just read, never an add or a
    /// subtract, so a raiser that read a count cannot overwrite a `Killed`
    /// that settles after its read. The answer is the lane's own. `between`
    /// runs after the share is taken and before the raise (just before the
    /// raise on a `Killed` cell). A share is held only inside this
    /// synchronous function, which awaits nothing, so every share is matched
    /// by the store or the give-back, a panic included.
    fn raise_kill(&self, lane: &StopLane, between: impl FnOnce()) -> RaiseResult {
        let mut seen = self.0.load(Ordering::SeqCst);
        let took_a_share = loop {
            if seen == Self::KILLED {
                break false;
            }
            debug_assert!(seen + 1 < Self::KILLED, "more raisers than the cell counts");
            match self
                .0
                .compare_exchange_weak(seen, seen + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break true,
                Err(now) => seen = now,
            }
        };
        let mut share = took_a_share.then_some(RaiserShare {
            cell: &self.0,
            phase: SharePhase::Taken,
        });
        between();
        if let Some(share) = share.as_mut() {
            share.phase = SharePhase::Raising;
        }
        let result = lane.try_raise(StopReason::Kill);
        if let Some(share) = share.as_mut() {
            share.phase = SharePhase::Settled {
                accepted: result != RaiseResult::Refused,
            };
        }
        #[cfg(test)]
        run_after_raise();
        result
    }

    /// The cell's raw value: the count of raisers in flight, or `Killed`.
    #[cfg(test)]
    pub(crate) fn cell(&self) -> u16 {
        self.0.load(Ordering::SeqCst)
    }

    /// Whether the `Kill` step was taken, or is being taken, by a raise of the
    /// same call.
    pub(crate) fn kill_started(&self) -> bool {
        self.0.load(Ordering::SeqCst) != Self::REQUESTED
    }

    /// What a `Stopped` the actor publishes reads as at the step reached,
    /// once a step in progress has settled.
    pub(crate) async fn requested(&self) -> StopOutcome {
        loop {
            match self.0.load(Ordering::SeqCst) {
                Self::KILLED => return StopOutcome::Killed,
                Self::REQUESTED => return StopOutcome::Graceful,
                _ => tokio::task::yield_now().await,
            }
        }
    }
}

/// Where a raiser is in its raise.
#[derive(Clone, Copy)]
enum SharePhase {
    /// The share is held and `between` runs: no `Kill` is written yet.
    Taken,
    /// `between` returned and the lane's raise is entered: the lane may have
    /// written the `Kill` before an unwind out of it.
    Raising,
    /// The lane's answer is known.
    Settled { accepted: bool },
}

/// One raiser's share of the step cell's count, settled when dropped, which
/// settles it whether the step ended or unwound: a raiser whose `Kill` stands
/// or may stand stores `Killed`, any other gives back its one count. A
/// `Killed` cell is left alone by the give-back.
struct RaiserShare<'a> {
    cell: &'a AtomicU16,
    phase: SharePhase,
}

impl Drop for RaiserShare<'_> {
    fn drop(&mut self) {
        match self.phase {
            SharePhase::Raising | SharePhase::Settled { accepted: true } => {
                self.cell.store(StopStep::KILLED, Ordering::SeqCst);
            }
            SharePhase::Taken | SharePhase::Settled { accepted: false } => {
                let mut seen = self.cell.load(Ordering::SeqCst);
                while seen != StopStep::KILLED {
                    debug_assert!(seen > 0, "a share is given back that was not taken");
                    if seen == StopStep::REQUESTED {
                        break;
                    }
                    match self.cell.compare_exchange_weak(
                        seen,
                        seen - 1,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => break,
                        Err(now) => seen = now,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    static AFTER_RAISE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Arms `hook` to run once on this thread, right after the next
/// [`StopStep::raise_kill`] raise and before its share is settled.
#[cfg(test)]
pub(crate) fn run_once_after_the_next_raise(hook: impl FnOnce() + 'static) {
    AFTER_RAISE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn run_after_raise() {
    let hook = AFTER_RAISE.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

/// Force-stop wait shared by `kill_by_id` and the concurrent sweep: the
/// target's kill grace -> `abort` (when there is a task to abort) -> settle
/// wait. `requested` is what a `Stopped` the actor publishes itself reads as
/// (`Killed` once a `Kill` was raised on it).
async fn force_stop(
    mut status_rx: StatusRx,
    abort: Option<AbortHandle>,
    record: AbortRecord,
    kill_grace: Duration,
    requested: StopOutcome,
) -> StopReport {
    if wait_terminal(&mut status_rx, kill_grace).await {
        return terminal_report(&status_rx, requested);
    }

    if let Some(abort) = &abort {
        record.abort(abort);
    }
    // Without an abort of its own the force follows the one the reaper
    // issues: no verdict before that abort plus the settle time. A wait for
    // an owed abort is capped by the force's whole budget once more.
    let cap = kill_grace.saturating_add(ABORT_SETTLE);
    if wait_end_or_floor(&mut status_rx, Some(&record), ABORT_SETTLE, cap).await {
        return terminal_report(&status_rx, requested);
    }

    unresponsive()
}

/// Waits for `status_rx` to reach a terminal state, counting `budget` from
/// now, and returns whether it did before the verdict floor of
/// [`rules::verdict_floor`]: the later of the budget's deadline and the
/// first abort recorded against the incarnation plus the settle time, held
/// while an abort is owed to it and not issued yet (up to `cap` past the
/// budget's deadline, plus the settle time of an abort issued inside the
/// cap). The wait wakes when the status reaches `Stopped`, when an abort is
/// recorded, and when the floor it last read is reached, and reads the record
/// again at each wake, so an abort that became owed in the meantime still
/// holds the verdict. A mark released while the wait sleeps is seen at the
/// next of those wakes, so the verdict can come later than the floor that
/// release implies, never earlier. With no record and nothing owed, the verdict comes
/// at the budget's deadline exactly.
pub(crate) async fn wait_end_or_floor(
    status_rx: &mut StatusRx,
    record: Option<&AbortRecord>,
    budget: Duration,
    cap: Duration,
) -> bool {
    let budget = bounded_wait(budget);
    let cap = bounded_wait(cap);
    let budget_at = saturating_deadline(Instant::now(), budget);
    let floor = || {
        let (aborted_at, owed) = record.map_or((None, false), AbortRecord::view);
        Instant::from_std(rules::verdict_floor(
            budget_at.into_std(),
            cap,
            aborted_at.map(Instant::into_std),
            owed,
        ))
    };
    loop {
        // Created before the record is read below (a `Notified` is woken by
        // every `notify_waiters` after its creation), so a record
        // written after that read wakes this pass.
        let mut recorded = std::pin::pin!(record.map(AbortRecord::recorded));
        if let Some(notified) = recorded.as_mut().as_pin_mut() {
            notified.enable();
        }
        let at = floor();
        let floor_passed = tokio::select! {
            biased;
            _ = status_rx.wait_for(|v| v.status == ActorStatus::Stopped) => return true,
            () = async {
                match recorded.as_mut().as_pin_mut() {
                    Some(notified) => notified.await,
                    None => std::future::pending().await,
                }
            } => false,
            () = tokio::time::sleep_until(at) => Instant::now() >= floor(),
        };
        if floor_passed {
            return read_status(status_rx) == ActorStatus::Stopped;
        }
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
struct ReaperSlot {
    tx: ReaperFeed,
    task: AbortHandle,
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
    deadline: Instant,
    abort: AbortHandle,
    /// The target's abort record: owed from the moment the entry exists
    /// (nothing is owed to a task that already finished), written when the
    /// entry aborts the task.
    record: AbortRecord,
    /// The step cell of the bounded stop whose ladder this entry carries on:
    /// the first stage writes it in the step that raises the `Kill`.
    step: Option<Arc<StopStep>>,
    fired: bool,
}

impl ReaperEntry {
    pub(crate) fn new(deadline: Instant, abort: AbortHandle, record: AbortRecord) -> Self {
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
    fn due(&self) -> Instant {
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
    fn fire(mut self) -> Option<Self> {
        if !self.abort.is_finished() {
            if let Some((_, lane, grace)) = self.kill_at.take() {
                // The entry holds the abort handle itself, so no deadline is
                // owed to an attach: a duplicate arm by one is harmless.
                // Decided by the lane's own answer to this raise, never by a
                // second read of the lane: a seal between the raise and a
                // later read would hide a `Kill` the lane accepted.
                let result = match &self.step {
                    Some(step) => step.raise_kill(&lane, || {}),
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
fn reaper_loop(rx: mpsc::UnboundedReceiver<ReaperEntry>) -> impl std::future::Future<Output = ()> {
    let state = ReaperState {
        rx,
        heap: BinaryHeap::new(),
    };
    reaper_run(state)
}

/// Pops the entry that is due and fires it. An entry with a stage left comes
/// back and is queued again before anything else runs, so no entry is ever
/// held across an await.
fn fire_due(heap: &mut BinaryHeap<ReaperEntry>) {
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

// ---------------------------------------------------------------------------
// Unit tests (pub(crate) internals, Rust Book Ch 11.3)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod cause_tests;

#[cfg(test)]
mod duplicate_id_registration_tests;

#[cfg(test)]
mod kill_raiser_tests;

#[cfg(test)]
mod observer_pairing_tests;

#[cfg(test)]
mod process_map_tests;

#[cfg(test)]
mod refused_raiser_tests;

#[cfg(test)]
mod shutdown_walk_tests;

#[cfg(all(test, feature = "tracing"))]
mod sweep_tick_tests;

#[cfg(all(test, feature = "tracing"))]
mod sweep_identity_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StatusValue;
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

    // Both registry maps are built with the same shard count, whatever CPUs
    // the process may use: a table growth then rehashes a small slice of the
    // entries instead of a quarter or a sixteenth of them. The literal bound
    // keeps the count itself from being lowered without this test noticing.
    #[tokio::test]
    async fn registry_maps_keep_the_shard_count_on_any_cpu_set() {
        let sys = ActorSystem::create(format!("shards-{}", uuid::Uuid::new_v4())).unwrap();
        for (map, shards) in [
            ("by_name", sys.by_name.shards().len()),
            ("by_id", sys.by_id.shards().len()),
        ] {
            assert_eq!(shards, REGISTRY_SHARDS, "{map} shard count");
            assert!(shards >= 256, "{map} has only {shards} shards");
        }
        assert_eq!(registry_map::<u8, u8>().shards().len(), REGISTRY_SHARDS);
    }

    // The longest single insert while a map takes `keys` one by one, each
    // timed alone; the map is dropped before the caller builds the next one.
    fn longest_insert<V>(
        map: DashMap<String, V>,
        keys: &[String],
        value: impl Fn() -> V,
    ) -> Duration {
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
            AbortRecord::new(),
        );
        drop(entry);
        assert!(ended(child).await.unwrap_err().is_cancelled());

        let child = tokio::spawn(std::future::pending::<()>());
        let entry = ReaperEntry::new(Instant::now(), child.abort_handle(), AbortRecord::new());
        entry.fire();
        assert!(ended(child).await.unwrap_err().is_cancelled());

        let done = tokio::spawn(async {});
        let abort = done.abort_handle();
        done.await.unwrap();
        drop(ReaperEntry::new(Instant::now(), abort, AbortRecord::new()));
    }

    /// A task that never ends on its own, with a lane to raise on.
    fn parked_with_a_lane() -> (tokio::task::JoinHandle<()>, StopLane, crate::types::LaneRx) {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sweep_ends_unasked_and_a_want_in_its_exit_window_restarts_it_multi_thread() {
        the_sweep_ends_unasked_and_a_want_in_its_exit_window_restarts_it().await;
    }

    // - Turn-watchdog sweep latch ---------------------------------------------

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
            + size_of::<Box<dyn Any + Send + Sync>>()
            + size_of::<StopLane>()
            + size_of::<StatusRx>()
            + size_of::<crate::types::TurnClock>()
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
                Some(&Arc::from(name.as_str())),
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
        let (hook_id, hook_name) = (id.clone(), name.clone());
        *sys.after_kill_lookup.lock().unwrap() = Some(Arc::new(move || {
            hook_sys.attach_abort(&hook_id, Some(&hook_name), abort.clone(), seq);
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
                None,
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
        sys.attach_abort(&id, None, task.abort_handle(), seq);
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

    /// Registers an incarnation under `id` (and `name`): a live lane and
    /// status plane owned by a pending task that also holds the registration
    /// guard, so the task's end unregisters it. Its abort handle is stored.
    fn register_incarnation(
        sys: &Arc<ActorSystem>,
        id: &ActorId,
        name: Option<&str>,
        grace: Duration,
    ) -> (u64, tokio::task::JoinHandle<()>) {
        let shared: Option<Arc<str>> = name.map(Arc::from);
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
                shared.as_ref(),
                &handle,
                true,
                TurnClock::default(),
                grace,
                Handle::current(),
            )
            .unwrap();
        let guard = RegistryGuard::new(sys.clone(), id.clone(), shared, seq);
        let task = tokio::spawn(async move {
            let _owned = (status_tx, stop_rx, guard);
            std::future::pending::<()>().await;
        });
        sys.attach_abort(id, name, task.abort_handle(), seq);
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
        let (seq, old) = register_incarnation(&sys, &id, Some(&name), SLOW_GRACE);
        let successor: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
        {
            let (hook_sys, hook_id, hook_name, slot) =
                (sys.clone(), id.clone(), name.clone(), successor.clone());
            *sys.after_kill_raise.lock().unwrap() = Some(Arc::new(move || {
                hook_sys.unregister_by_id(&hook_id, seq);
                hook_sys.unregister_by_name(&hook_name, seq);
                // A shutdown refuses registrations; the successor stands for
                // the next incarnation of the same id.
                let phase = hook_sys
                    .phase
                    .swap(SystemPhase::Active.as_u8(), Ordering::SeqCst);
                let (_, task) =
                    register_incarnation(&hook_sys, &hook_id, Some(&hook_name), HOOK_GRACE);
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
        let (_seq, aborted_task) = register_incarnation(
            &sys,
            &aborted_id,
            Some("sweep-aborted"),
            Duration::from_millis(40),
        );
        let (_seq, parked_task) = register_incarnation(
            &sys,
            &parked_id,
            Some("sweep-parked"),
            Duration::from_millis(100),
        );
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
        let id = ActorId::from("stale-attach-id");
        let name = "stale-attach-name";
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
                    Some(&Arc::from(name)),
                    &handle,
                    true,
                    TurnClock::default(),
                    Duration::from_secs(60),
                    Handle::current(),
                )
                .unwrap();
            (seq, stop_lane)
        };
        // The first incarnation registers, and its entries are freed before
        // its attach runs.
        let (old_seq, _old_lane) = register(&sys);
        sys.unregister_by_id(&id, old_seq);
        sys.unregister_by_name(name, old_seq);
        // A later spawn registers the same id and name and attaches its own
        // abort; its lane then holds a `Kill`.
        let (new_seq, new_lane) = register(&sys);
        let new_task = tokio::spawn(std::future::pending::<()>());
        sys.attach_abort(&id, Some(name), new_task.abort_handle(), new_seq);
        new_lane.raise(StopReason::Kill);
        let arms = sys.backstop_arms.load(Ordering::SeqCst);
        let old_task = tokio::spawn(std::future::pending::<()>());
        sys.attach_abort(&id, Some(name), old_task.abort_handle(), old_seq);
        let by_id = sys
            .by_id
            .get(&id)
            .and_then(|entry| entry.abort.as_ref().map(|a| a.id()));
        let by_name = sys
            .by_name
            .get(name)
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
                    None,
                    &handle,
                    is_root,
                    TurnClock::default(),
                    grace,
                    Handle::current(),
                )
                .unwrap();
            sys.phase.store(phase, Ordering::SeqCst);
            let guard = RegistryGuard::new(sys.clone(), id.clone(), None, seq);
            let task = tokio::spawn(async move {
                let _owned = stop_rx;
                std::future::pending::<()>().await;
            });
            sys.attach_abort(id, None, task.abort_handle(), seq);
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
    const CUT_IN_STEP_ONE: (Duration, Duration) =
        (Duration::from_millis(100), Duration::from_secs(30));

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
                        let first =
                            Scripted::register(&hook_sys, &ActorId::from("late-first"), grace);
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
                        let late_root = Scripted::register(
                            &hook_sys,
                            &ActorId::from("late-killed"),
                            LONG_GRACE,
                        );
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
                while !matches!(root.lane.reason(), Some(StopReason::Kill))
                    && Instant::now() < bound
                {
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
                    let late_root = Scripted::register(
                        &hook_sys,
                        &ActorId::from("left-registered-late"),
                        grace,
                    );
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
            None,
            &handle,
            true,
            TurnClock::default(),
            LONG_GRACE,
            rt.clone(),
        )?;
        let guard = RegistryGuard::new(sys.clone(), id.clone(), None, seq);
        let task = rt.spawn(async move {
            let _ = stop_rx.changed().await;
            drop(guard);
            StatusValue::write_reason(&status_tx, crate::types::StopReason::Graceful, true);
        });
        sys.attach_abort(id, None, task.abort_handle(), seq);
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
            let (hook_sys, in_window, saw) =
                (sys.clone(), in_window.clone(), saw_the_shutdown.clone());
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
        let leftover =
            Scripted::register_as(&sys, &ActorId::from("held-leftover"), LONG_GRACE, false);
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
        let leftover =
            Scripted::register_as(&sys, &ActorId::from("final-early"), LONG_GRACE, false);
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
            let system =
                ActorSystem::create(format!("late-kill-{}", uuid::Uuid::new_v4())).unwrap();
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
        use crate::actor::subtree::SubtreeLink;
        use crate::types::StopLane;
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
}
