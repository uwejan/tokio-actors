//! The `Kill` path: the declared deadline, the shared raiser, the step cell of a bounded stop and the waits on an actor's end.

use std::sync::atomic::{AtomicU16, AtomicU8, Ordering};
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::Instant;

#[cfg(test)]
use super::ReaperSlot;
use super::{ActorSystem, ReaperEntry, RegKey};
use crate::actor::handle::emit_stop_requested;
use crate::actor::incarnation::{IncarnationRef, RaiseResult, StatusRx, StopLane};
use crate::actor::runtime::{bounded_wait, saturating_deadline};
use crate::actor::supervision::rules;
#[cfg(test)]
use crate::system::{AFTER_RAISE, BEFORE_KILL_RAISE};
use crate::telemetry::{EventKind, KillStepKind, TelemetryEvent};
use crate::types::{ActorId, ActorStatus, StopOutcome, StopReason};

/// Queues the `KillStep` event `step` of the incarnation whose abort record is
/// `record`, in the system in hand or the one the record names. It builds
/// nothing when no sink takes the event or the incarnation has no recorded
/// identity, never waits and runs no user code, so the reaper, a drop and the
/// raiser call it with no care for what they hold. `after` is read only when
/// the event is built.
#[inline]
pub(super) fn push_kill_step(
    record: &IncarnationRef,
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
/// queued by [`IncarnationRef::abort`] under the record's lock for the abort that
/// writes the record's first instant, so the step is queued once per
/// incarnation whichever abort (the reaper's, a caller's force path, a
/// ladder's, a start bound's) finds the task unfinished first. Takes no lock
/// of the record, queues nothing, and builds nothing unless a sink takes the
/// kind.
pub(crate) fn stage_abort_fired(record: &IncarnationRef) -> Option<crate::telemetry::Staged> {
    crate::telemetry::stage_about(record, None, EventKind::KillStep, |actor| {
        TelemetryEvent::KillStep {
            actor: actor.clone(),
            step: KillStepKind::AbortFired,
            after: None,
        }
    })
}

/// The time from now to `deadline`.
pub(super) fn step_deadline_in(deadline: Instant) -> Option<Duration> {
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

/// Where a registry `Kill` comes from, which decides its deadline and how it
/// is raised.
pub(crate) enum KillFrom<'a> {
    /// A plain `Kill`: the deadline is the entry's kill grace from now.
    Plain,
    /// A plain `Kill` whose deadline is this window from now.
    Window(Duration),
    /// The `Kill` step of a bounded stop, raised through the call's step
    /// cell, which counts the raiser in flight first. The deadline is the
    /// entry's kill grace from now.
    Step(&'a StopStep),
    /// A `Kill` raised on behalf of an exit signal's reason: the lane stores
    /// it when it holds no `Kill` yet, and the actor ends with it in place of
    /// `Kill`. The deadline is the entry's kill grace from now.
    Caused(StopReason),
}

/// The one way a `Kill` is raised on an actor the registry holds. In this
/// order, it declares the deadline of the `Kill` on the registry entry `key`
/// names (the window of [`KillFrom::Window`] from now, otherwise the entry's
/// kill grace), raises the `Kill` on `lane` as `from` says, and, when the
/// raise is accepted, schedules the abort of the task through the reaper of
/// the entry's own runtime. The entry is matched by `status_rx`, never by the
/// id or name alone, so a later incarnation under the same id or name is
/// neither declared on nor armed. An entry whose abort handle is not attached
/// yet is armed by the attach, after the earliest deadline declared on it.
///
/// For [`KillFrom::Step`] the step cell counts the raiser in flight before
/// the raise. With no `system` (the caller found none) nothing is declared
/// and nothing is armed: the `Kill` is still raised.
///
/// Never call it while holding a registry guard: the declaration takes a
/// write lock on the same shard.
pub(crate) fn raise_kill_declared(
    system: Option<&ActorSystem>,
    key: KillKey<'_>,
    status_rx: &StatusRx,
    lane: &StopLane,
    from: KillFrom<'_>,
) -> KillRaised {
    let window = match from {
        KillFrom::Window(window) => Some(window),
        KillFrom::Plain | KillFrom::Step(_) | KillFrom::Caused(_) => None,
    };
    if let Some(system) = system {
        let declared = match key {
            KillKey::Id(id) => system.declare_kill(id, status_rx, window),
            KillKey::Name(name) => system.declare_kill(name, status_rx, window),
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
    let noted_cause = match &from {
        KillFrom::Caused(cause) => Some(cause.clone()),
        KillFrom::Plain | KillFrom::Window(_) | KillFrom::Step(_) => None,
    };
    let result = match from {
        KillFrom::Step(step) => step.raise_shared(lane, between_declaration_and_raise),
        KillFrom::Caused(cause) => {
            between_declaration_and_raise();
            lane.try_raise_kill_caused(cause)
        }
        KillFrom::Plain | KillFrom::Window(_) => {
            between_declaration_and_raise();
            lane.try_raise(StopReason::Kill)
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

/// The point of [`raise_kill_declared`] after the declaration and before the
/// raise (for [`KillFrom::Step`], after the step cell counted the raiser).
/// Under test it runs the hook armed on this thread by
/// [`run_once_before_the_next_kill_raise`]; otherwise nothing runs.
#[inline(always)]
fn between_declaration_and_raise() {
    #[cfg(test)]
    {
        let hook = BEFORE_KILL_RAISE.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Arms `hook` to run once on this thread, inside the next
/// [`raise_kill_declared`], after its declaration and before its raise.
#[cfg(test)]
pub(crate) fn run_once_before_the_next_kill_raise(hook: impl FnOnce() + 'static) {
    BEFORE_KILL_RAISE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

impl ActorSystem {
    /// The earliest `Kill` deadline declared on the registration
    /// `registration_seq` under `id`, `0` when none was.
    pub(super) fn declared_kill_due(&self, id: &ActorId, registration_seq: u64) -> u64 {
        self.by_id
            .get(id)
            .filter(|entry| entry.registration_seq == registration_seq)
            .map_or(0, |entry| entry.kill_due)
    }

    /// Nanoseconds from the system's epoch to now, at least `1` so that `0`
    /// stays free for "no deadline".
    pub(super) fn kill_clock(&self) -> u64 {
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
    /// shard's lock. `key` is the id, or the name a raiser found its target
    /// by, which is the same key: the status plane decides either way.
    pub(super) fn declare_kill<Q>(
        &self,
        key: &Q,
        status_rx: &StatusRx,
        window: Option<Duration>,
    ) -> bool
    where
        RegKey: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let declared = self.declare_on(key, status_rx, window);
        #[cfg(test)]
        self.run_after_kill_declaration();
        declared
    }

    /// Declares on the entry under `key` when it is the incarnation of
    /// `status_rx`, under that one shard's lock and nothing else.
    fn declare_on<Q>(&self, key: &Q, status_rx: &StatusRx, window: Option<Duration>) -> bool
    where
        RegKey: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let now = self.kill_clock();
        let Some(mut entry) = self.by_id.get_mut(key) else {
            return false;
        };
        if !entry.status_rx.same_channel(status_rx) {
            return false;
        }
        entry.declare_kill(now, window);
        true
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
        record: &IncarnationRef,
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
        record: &IncarnationRef,
        window: Option<Duration>,
    ) -> Option<AbortHandle> {
        let (abort, grace, runtime) = match key {
            KillKey::Id(id) => self.kill_target_at(id, raised_on)?,
            KillKey::Name(name) => self.kill_target_at(name, raised_on)?,
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
    pub(super) fn run_after_kill_lookup(&self) {
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
        self.kill_target_at(id, status_rx)
    }

    /// [`kill_target_for`](Self::kill_target_for) for the entry under `key`,
    /// an id or the name it was found by: one guard, dropped before this
    /// returns, and the entry matched by its status plane.
    fn kill_target_at<Q>(
        &self,
        key: &Q,
        status_rx: &StatusRx,
    ) -> Option<(Option<AbortHandle>, Duration, Handle)>
    where
        RegKey: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let entry = self.by_id.get(key)?;
        if !entry.status_rx.same_channel(status_rx) {
            return None;
        }
        Some((entry.abort.clone(), entry.kill_grace, entry.runtime.clone()))
    }
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

/// The answer to a spawn whose start acknowledgment closed unanswered, read
/// from the actor's status plane once its reason is final, so every reader
/// of a closed acknowledgment gives one answer: a `Failure` is
/// `SpawnError::Init` with it, an ignored start is `SpawnError::Ignored`, and
/// any other end (a `Kill`, an abort) is `SpawnError::Killed`.
pub(crate) async fn closed_start_error(rx: &mut StatusRx) -> crate::error::SpawnError {
    use crate::error::SpawnError;
    match wait_final_reason(rx).await {
        StopReason::Failure(err) => SpawnError::Init(Box::new(err)),
        _ if rx.ended_ignored() => SpawnError::Ignored,
        _ => SpawnError::Killed,
    }
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
    pub(super) const REQUESTED: u16 = 0;
    /// A `Kill` of the call was accepted; never left again.
    pub(super) const KILLED: u16 = u16::MAX;

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
    pub(super) fn raise_shared(&self, lane: &StopLane, between: impl FnOnce()) -> RaiseResult {
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

/// Arms `hook` to run once on this thread, right after the next
/// [`StopStep::raise_shared`] raise and before its share is settled.
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
    record: Option<&IncarnationRef>,
    budget: Duration,
    cap: Duration,
) -> bool {
    let budget = bounded_wait(budget);
    let cap = bounded_wait(cap);
    let budget_at = saturating_deadline(Instant::now(), budget);
    let floor = || {
        let (aborted_at, owed) = record.map_or((None, false), IncarnationRef::view);
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
        let mut recorded = std::pin::pin!(record.map(IncarnationRef::recorded));
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
pub(super) type LadderHook = Arc<dyn Fn(LadderPoint, &ActorId) + Send + Sync>;
