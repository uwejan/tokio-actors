//! Actor task spawning and the message-dispatch run loop.
//!
//! The run loop's `select!` is `biased`, highest priority first: the stop lane,
//! then the system channel (`GetStatus`), then the supervision restart plane
//! (restart attempts completing), then the death plane (child runner
//! completions), then the verdict signal (an incarnation given the Unresponsive
//! verdict), then the forwarder plane (timer and stream completions), then the
//! priority lane (present only on an actor spawned with one), then the user
//! mailbox. This ordering is deliberate, not incidental: it mirrors the OTP
//! spirit of signals and supervisor traffic pre-empting ordinary message-queue
//! processing, so a stop/kill request is honored, a child's death is observed,
//! and a priority message is received even while the mailbox is full, and
//! `get_status` answers without waiting behind user traffic. Every branch is
//! only ever observed at a turn boundary - between two callbacks, never during
//! one - so none of this races or cancels a callback already in progress (the
//! Isolated Turn Principle).
//!
//! The priority lane is served ahead of the mailbox at every turn, so a
//! priority flood at or above the rate the actor consumes it starves the normal
//! mailbox for as long as it lasts (the lane's capacity bounds its memory and
//! parks a flooding sender, and no message is lost). A sustained storm of
//! supervision events, which sit above the lane, delays the lane and the
//! mailbox alike. Reception order, highest first: stop lane, system channel,
//! restart plane, death plane, verdict signal, forwarder plane, priority lane,
//! mailbox; a stop request never travels on the priority lane.
//!
//! A restart attempt runs in the parent's restart plane (a
//! `JoinSet<RestartOutcome>`), initiated by the run loop or by a handler (a
//! bounce, `restart_child`): the run loop never blocks on one. An attempt first
//! waits, still in the restart plane, until the previous incarnation's whole
//! subtree has ended (every actor it spawned, and every actor those spawned),
//! bounded by the longest chain of stop budgets in the subtree (a worker's kill
//! grace, a supervisor child's `Shutdown` window plus its kill grace), each
//! followed by a short settle time, plus one more settle time, and unbounded
//! only while a supervisor child under `Shutdown::Infinity` runs in it or a
//! budget is too large to represent. Past that bound it gives no verdict on an
//! actor before the abort the system owes it has had the settle time to end
//! it, waiting for an abort that is late for at most the bound once more plus
//! one more settle time. It honors a stop order during that wait.
//! The attempt then owns its incarnation from the factory to the init ack,
//! honoring a stop order at every stage, and hands the incarnation over with
//! its outcome; the loop then spawns the incarnation's runner in the death
//! plane, which owns it to its fate. An attempt's outcome - adopted, cancelled
//! before its factory ran, stopped under an order, reported as an ordinary
//! failure, or reported unresponsive because its wait for the previous subtree
//! outlived its bound - is the `JoinSet`'s own returned value; nothing about a
//! restart travels over any channel of any capacity. An attempt whose wait
//! expired is not evaluated: it is recorded as down, and the supervisor stops
//! with an unresponsive-child failure, so its own supervisor decides.
//!
//! The stop lane itself is a `watch` signal, not a channel: raising it is
//! synchronous and infallible, severity only ever increases, and multiple
//! raises that land between two turn boundaries coalesce into the single
//! highest-severity, latest state - the loop is guaranteed at least one fresh
//! observation per distinct severity level reached, never one per individual
//! raise.
//!
//! A supervised child's death is delivered through its own per-incarnation fate
//! cell, populated by that incarnation's runner task living in the supervisor's
//! death plane (a `JoinSet`), never through a shared channel of any bounded
//! capacity - so however many children die in the same instant, none of their
//! fates are lost waiting for room. The runner, the death-plane task that
//! awaits the incarnation's exit, also owns that incarnation's stop ladder: any
//! stop committed for the incarnation, from any origin, is escalated by the
//! runner, so no caller future and no detached timer owns any step of it. A
//! task dying with `StopReason::Kill` never awaits its own children: it drops
//! its supervision state, and each live child's link guard acts for itself. A
//! worker has Kill raised on its lane and is aborted by the system's reaper
//! once its kill grace has passed. A child that is itself a supervisor is given
//! `StopReason::ParentKilled`; it runs `on_stopped` and stops its own children
//! in order within its own `Shutdown` window, the reaper raises Kill at the end
//! of that window and aborts the task once the child's kill grace has passed,
//! and a supervisor child under `Shutdown::Infinity` has no window and is not
//! aborted. A child whose lane already holds a Kill when its guard fires keeps
//! that Kill, whatever its `Shutdown`, and is aborted once its kill grace has
//! passed.
//!
//! When the message loop of a supervisor ends for any reason other than a
//! kill, a final drain runs between the loop and `on_stopped`: every child
//! death still pending (not yet drained, a failed restart attempt, deferred
//! behind a pending group restart, held as a manual stop's snapshot, or left
//! in a completing group's worklist) is collected and delivered once to
//! `on_child_stopped`, with no restart and no charge. Then the supervisor's
//! teardown stops its children, each under its own `Shutdown`, one at a time,
//! newest first; a `SimpleOneForOne` supervisor orders all of its children at
//! once. A Kill raised during the teardown ends it, and the children still
//! running take the Kill path.
//!
//! Every actor, supervisor or not, owns a forwarder plane (also a `JoinSet`)
//! holding its timer and stream forwarder tasks. Draining it is folded into
//! the same combined branch as the supervision planes, biased last: a fired
//! one-shot, a finished stream, or a forwarder that panicked is reaped there,
//! which frees its registration and - for a panic - records it as the
//! actor's last forwarder error. None of this ever stops the actor. At the
//! actor's end the forwarder plane is aborted and every forwarder's future,
//! with the stream or timer it owns, is dropped before the actor's status
//! reads as ended; an abort of the actor's task tears the plane down with the
//! context, and each forwarder holds the actor's status plane and subtree
//! open until its own future is dropped.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinError, JoinHandle, JoinSet};
use tokio::time::{Duration, Instant};

use crate::actor::panic::{catch_callback, catch_sync, payload_into_string};
use crate::actor::subtree::{SubtreeHold, SubtreeLink, SubtreeWatch};
use crate::actor::supervision::rules::{init_kill_check, InitStep, TeardownAction};
use crate::actor::supervision::{
    advance_restart_chain, await_cell, evaluate_strategy, rules, run_ladder, CellOutcome,
    CellWatch, ChainStep, ChildLifecycle, ChildLinkGuard, ChildState, DeathOutcome, DownPending,
    GroupMembership, GroupPhase, GroupRestart, GroupScope, Origin, ReportClass, RestartFn,
    RestartOutcome, Route, SlotIo, StrategyOutcome, SupervisionConfig, SupervisionState, TaskPlane,
    TaskState, DEFAULT_KILL_GRACE,
};
use crate::actor::{
    context::{ActorContext, ForwarderEnd},
    handle::ActorHandle,
    Actor, ActorEnvelope,
};
use crate::error::{ActorError, SpawnError, SupervisionError};
use crate::system::{ActorSystem, RegistryGuard};
use crate::types::{
    ActorId, ActorStatus, ActorStatusInfo, ChildEvent, ChildFate, ChildStoppedInternal, Envelope,
    LaneState, RestartStrategy, RestartType, Shutdown, StatusValue, StopLane, StopReason,
    StopSeverity, SupervisionAction, SystemMessage, TurnClock,
};

/// Configuration for the actor's mailbox.
#[derive(Debug, Clone)]
pub struct MailboxConfig {
    /// The maximum number of messages the mailbox can hold. A value above
    /// [`tokio::sync::Semaphore::MAX_PERMITS`] is clamped to it at spawn
    /// time instead of being rejected.
    pub capacity: usize,
    /// The priority lane's bounded capacity. `None` (the default) means the
    /// actor has no priority lane at all, in which case every `*_priority`
    /// method on its handle behaves exactly like its normal-lane sibling.
    /// There is no default capacity for an enabled lane: see
    /// [`with_priority_lane`](Self::with_priority_lane).
    pub priority_capacity: Option<usize>,
}

impl Default for MailboxConfig {
    fn default() -> Self {
        Self {
            capacity: 64,
            priority_capacity: None,
        }
    }
}

impl MailboxConfig {
    /// Sets the mailbox capacity. A value above
    /// [`tokio::sync::Semaphore::MAX_PERMITS`] is clamped to it at spawn
    /// time instead of being rejected.
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Enables a priority lane with the given bounded capacity: a second,
    /// independent mailbox that the four `*_priority`
    /// [`ActorHandle`] methods send on,
    /// received ahead of the normal mailbox.
    ///
    /// Erlang/OTP's own guidance applies: "You very seldom need to resort to
    /// usage of priority messages. Receiving processes have not been
    /// optimized for handling large amounts of priority messages." (Erlang
    /// Reference Manual, Processes)
    ///
    /// # Ordering
    /// Messages keep their send order within each lane. A priority message is
    /// received before every normal message still queued when the actor next
    /// picks a message, including normal messages the same sender sent earlier.
    /// No message, priority or not, interrupts a callback already in progress.
    /// The run loop receives, highest first: the stop lane, the system channel,
    /// the supervision branches (restart plane, death plane, verdict signal,
    /// forwarders), this lane, and the normal mailbox; a stop request never
    /// travels on this lane.
    ///
    /// # Starvation
    /// The lane is served ahead of the normal mailbox at every turn, so a
    /// priority flood at or above the rate the actor consumes it starves the
    /// normal mailbox for as long as it lasts. The lane's capacity bounds
    /// its memory and parks a sender that fills it, but nothing gives the
    /// normal mailbox a share, and no message is lost. A sustained storm of
    /// supervision events, which sit above the lane, delays the lane and the
    /// normal mailbox alike.
    ///
    /// Leaving this unset means the actor has no priority lane at all:
    /// every `*_priority` method on its handle then behaves exactly like its
    /// normal-lane sibling.
    ///
    /// # Divergences from Erlang
    /// A plain Erlang process has one message queue, whose length is not
    /// otherwise observable and which never applies backpressure. This lane
    /// diverges on both counts: its length and capacity are observable here
    /// ([`ActorHandle::priority_len`](crate::actor::handle::ActorHandle::priority_len),
    /// [`ActorHandle::priority_capacity`](crate::actor::handle::ActorHandle::priority_capacity)),
    /// and, being bounded, a full lane applies backpressure (`Full`, a
    /// timeout) instead of growing without limit.
    ///
    /// There is no default capacity, and a capacity of `0` is rejected at
    /// spawn time with
    /// [`SpawnError::ZeroMailboxCapacity`],
    /// the same error an empty ordinary mailbox is rejected with. A capacity
    /// above [`tokio::sync::Semaphore::MAX_PERMITS`] is clamped to it at
    /// spawn time instead.
    pub fn with_priority_lane(mut self, capacity: usize) -> Self {
        self.priority_capacity = Some(capacity);
        self
    }
}

/// Configuration for spawning an actor.
#[derive(Debug, Clone, Default)]
pub struct ActorConfig {
    /// Mailbox configuration.
    pub mailbox: MailboxConfig,
    /// Supervision configuration. `Some` if this actor is a supervisor.
    pub supervision: Option<SupervisionConfig>,
}

impl<'a> From<&'a ActorConfig> for ActorConfig {
    fn from(value: &'a ActorConfig) -> Self {
        value.clone()
    }
}

impl ActorConfig {
    /// Sets the mailbox capacity. A value above
    /// [`tokio::sync::Semaphore::MAX_PERMITS`] is clamped to it at spawn
    /// time instead of being rejected.
    pub fn with_mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox.capacity = capacity;
        self
    }

    /// Sets the complete mailbox configuration.
    pub fn with_mailbox(mut self, mailbox: MailboxConfig) -> Self {
        self.mailbox = mailbox;
        self
    }

    /// Enables a priority lane with the given bounded capacity (shorthand
    /// for [`MailboxConfig::with_priority_lane`] through
    /// [`with_mailbox`](Self::with_mailbox)): a second, independent mailbox
    /// that the four `*_priority`
    /// [`ActorHandle`] methods send on,
    /// received ahead of the normal mailbox.
    ///
    /// Erlang/OTP's own guidance applies: "You very seldom need to resort to
    /// usage of priority messages. Receiving processes have not been
    /// optimized for handling large amounts of priority messages." (Erlang
    /// Reference Manual, Processes)
    ///
    /// # Ordering
    /// Messages keep their send order within each lane. A priority message is
    /// received before every normal message still queued when the actor next
    /// picks a message, including normal messages the same sender sent earlier.
    /// No message, priority or not, interrupts a callback already in progress.
    /// The run loop receives, highest first: the stop lane, the system channel,
    /// the supervision branches (restart plane, death plane, verdict signal,
    /// forwarders), this lane, and the normal mailbox; a stop request never
    /// travels on this lane.
    ///
    /// # Starvation
    /// The lane is served ahead of the normal mailbox at every turn, so a
    /// priority flood at or above the rate the actor consumes it starves the
    /// normal mailbox for as long as it lasts. The lane's capacity bounds
    /// its memory and parks a sender that fills it, but nothing gives the
    /// normal mailbox a share, and no message is lost. A sustained storm of
    /// supervision events, which sit above the lane, delays the lane and the
    /// normal mailbox alike.
    ///
    /// Leaving this unset means the actor has no priority lane at all:
    /// every `*_priority` method on its handle then behaves exactly like its
    /// normal-lane sibling.
    ///
    /// # Divergences from Erlang
    /// A plain Erlang process has one message queue, whose length is not
    /// otherwise observable and which never applies backpressure. This lane
    /// diverges on both counts: its length and capacity are observable here
    /// ([`ActorHandle::priority_len`](crate::actor::handle::ActorHandle::priority_len),
    /// [`ActorHandle::priority_capacity`](crate::actor::handle::ActorHandle::priority_capacity)),
    /// and, being bounded, a full lane applies backpressure (`Full`, a
    /// timeout) instead of growing without limit.
    ///
    /// There is no default capacity, and a capacity of `0` is rejected at
    /// spawn time with
    /// [`SpawnError::ZeroMailboxCapacity`],
    /// the same error an empty ordinary mailbox is rejected with. A capacity
    /// above [`tokio::sync::Semaphore::MAX_PERMITS`] is clamped to it at
    /// spawn time instead.
    pub fn with_priority_lane(mut self, capacity: usize) -> Self {
        self.mailbox = self.mailbox.with_priority_lane(capacity);
        self
    }

    /// Makes the actor a SUPERVISOR with the default configuration
    /// (OneForOne, 3 restarts / 5s window).
    ///
    /// The actor takes the supervisor role; its children are the supervised
    /// ones.
    pub fn supervisor(mut self) -> Self {
        self.supervision = Some(SupervisionConfig::default());
        self
    }

    /// Enables supervision with a custom configuration.
    pub fn with_supervision(mut self, config: SupervisionConfig) -> Self {
        self.supervision = Some(config);
        self
    }
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Spawns an actor detached: no initialization ack, `_join` discarded. This
/// is the fire-and-forget spawn behind
/// [`SpawnBuilder::detached`](crate::actor::SpawnBuilder::detached).
pub(crate) fn into_actor<A: Actor>(
    id: impl Into<ActorId>,
    actor: A,
    config: impl Into<ActorConfig>,
    name: Option<String>,
    system: Option<Arc<ActorSystem>>,
    kill_grace: Duration,
) -> Result<ActorHandle<A>, SpawnError> {
    // Only reachable from the top-level `.detached()` spawn path: always a root.
    let (handle, _join, _subtree) = spawn_actor(
        id.into(),
        actor,
        config.into(),
        name,
        system,
        None,
        true,
        kill_grace,
        None,
    )?;
    Ok(handle)
}

/// Spawns an actor, returning the handle and the task's `JoinHandle`.
///
/// The task completes with the actor's final [`StopReason`]. For supervised
/// children the caller wraps the `JoinHandle` in a runner
/// ([`spawn_runner`]); the runner, not the dying actor, is the authoritative
/// death signal (a panicking actor cannot self-report).
///
/// `ack`, if present, is signalled exactly once from inside the task: `Ok(())`
/// right after the actor transitions to [`ActorStatus::Running`] (post
/// `on_started`, pre message loop), or `Err` once `pre_start`/`on_started`
/// failed AND the full teardown (including the registry guard drop) has run.
/// Callers that do not want to wait (supervised children, `.detached()`
/// spawns) pass `None`.
///
/// `is_root` distinguishes a top-level `SpawnBuilder` spawn (`true`) from a
/// supervised child (`false`, `spawn_child` and its restart path). System
/// shutdown only signals roots directly; a root's own supervision cascade
/// takes its children down in turn.
///
/// `kill_grace` is the time the actor is given, after a `Kill` is raised on
/// it, to reach its next turn boundary before its task is aborted. It is
/// stored in the registry entry together with the runtime that drives the
/// task, so every kill path that finds the actor reads both.
///
/// No clone of the actor's status sender leaves this function: the plane
/// behind [`ActorHandle::status`] closes when the task ends, and a child's
/// `Stopped` status is written only by its own task reaching its end, never
/// by anything watching it from the outside, so observing it always means the
/// task actually got there.
///
/// `subtree` attaches the actor to its parent's subtree token (`None` for a
/// root). The third element is an observer of the actor's own subtree: it
/// reads as ended once the actor and every actor it spawned have ended, and
/// never keeps any of them alive.
type SpawnOutcome<A> = (ActorHandle<A>, JoinHandle<StopReason>, SubtreeWatch);

/// What [`spawn_actor_parts`] returns: a [`SpawnOutcome`]'s parts with a
/// sender of the status plane.
type SpawnParts<A> = (
    ActorHandle<A>,
    JoinHandle<StopReason>,
    watch::Sender<StatusValue>,
    SubtreeWatch,
);

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_actor<A: Actor>(
    id: ActorId,
    actor: A,
    config: ActorConfig,
    name: Option<String>,
    system: Option<Arc<ActorSystem>>,
    ack: Option<oneshot::Sender<Result<(), ActorError>>>,
    is_root: bool,
    kill_grace: Duration,
    subtree: Option<SubtreeLink>,
) -> Result<SpawnOutcome<A>, SpawnError> {
    let (handle, join, _status, subtree) = spawn_actor_parts(
        id, actor, config, name, system, ack, is_root, kill_grace, subtree,
    )?;
    Ok((handle, join, subtree))
}

/// [`spawn_actor`], also returning a sender of the actor's status plane:
/// held by a caller, it keeps the plane open past the actor's own senders.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_actor_parts<A: Actor>(
    id: ActorId,
    actor: A,
    config: ActorConfig,
    name: Option<String>,
    system: Option<Arc<ActorSystem>>,
    ack: Option<oneshot::Sender<Result<(), ActorError>>>,
    is_root: bool,
    kill_grace: Duration,
    subtree: Option<SubtreeLink>,
) -> Result<SpawnParts<A>, SpawnError> {
    let mailbox_capacity = config.mailbox.capacity;
    if mailbox_capacity == 0 {
        return Err(SpawnError::ZeroMailboxCapacity);
    }
    if config.mailbox.priority_capacity == Some(0) {
        return Err(SpawnError::ZeroMailboxCapacity);
    }
    // Clamped, not rejected: a capacity above what the channel
    // implementation can represent is over-provisioning, not a
    // configuration error, so `usize::MAX` stays a valid "as large as this
    // can go" shorthand instead of panicking inside `mpsc::channel`.
    let mailbox_capacity = mailbox_capacity.min(tokio::sync::Semaphore::MAX_PERMITS);
    let priority_capacity = config
        .mailbox
        .priority_capacity
        .map(|cap| cap.min(tokio::sync::Semaphore::MAX_PERMITS));
    let handle = Handle::try_current().map_err(|_| SpawnError::MissingRuntime)?;

    // Every spawn always resolves to a concrete system (bare anonymous
    // spawns join `ActorSystem::default()` as roots, same as named spawns
    // already did) - resolved up front so the handle can carry the system's
    // stable identity, used by `ActorHandle`'s `Eq`/`Hash` alongside its
    // `ActorId`.
    let target_system = system.clone().unwrap_or_else(ActorSystem::default);

    let (tx, rx) = mpsc::channel(mailbox_capacity);
    // A priority lane is a second, independent bounded channel, built only
    // when the config asks for one; `priority_rx` travels into the run loop
    // below, `priority_tx` into the handle callers use to reach it.
    let (priority_tx, priority_rx) = match priority_capacity {
        Some(cap) => {
            let (tx, rx) = mpsc::channel(cap);
            (Some(tx), Some(rx))
        }
        None => (None, None),
    };
    let (system_tx, system_rx) = mpsc::channel::<SystemMessage>(64);
    let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Initializing));
    let (stop_lane, stop_rx) = StopLane::new();
    let abort_record = stop_lane.abort_record().clone();
    let actor_handle = ActorHandle::new(
        id.clone(),
        tx,
        system_tx,
        stop_lane,
        mailbox_capacity,
        status_rx,
        target_system.name().into(),
        priority_tx,
        Arc::new(AtomicBool::new(false)),
    );

    // Owned by the run loop (the writer) and the registry entry (the
    // reader) only - never by `ActorHandle`, so cloning a handle never
    // touches this clock's shared threshold cell.
    let turn_clock = TurnClock::new(target_system.turn_threshold_cell());

    let supervision = config.supervision.map(SupervisionState::new);
    let subtree = SubtreeHold::with_record(subtree, abort_record);
    let subtree_watch = subtree.watch();
    let mut context = ActorContext::new(
        id.clone(),
        actor_handle.clone(),
        handle.clone(),
        status_tx.clone(),
        system,
        name.clone(),
        supervision,
    );
    context.set_subtree(subtree.clone());
    context.set_turn_clock(turn_clock.clone());
    if let Some(sup) = context.supervision_ref() {
        sup.verdicts.set_supervisor(id.clone());
    }

    // The name-claim happens BEFORE the task is spawned; the AbortHandle
    // does not exist yet at this point (it only exists once `handle.spawn`
    // returns a JoinHandle), so it is attached in a second step just below.
    //
    // `ActorContext::new` above already ran with the ORIGINAL `system`
    // argument (possibly `None`), so a bare-spawned actor's `ctx.system`
    // stays `None`; this is harmless because every downstream spawn path
    // re-applies the same default-system fallback.
    let registration_seq = target_system.register_actor::<A>(
        &id,
        name.as_deref(),
        &actor_handle,
        is_root,
        turn_clock.clone(),
        kill_grace,
        handle.clone(),
    )?;
    let guard = Some(
        RegistryGuard::new(
            target_system.clone(),
            id.clone(),
            name.clone(),
            registration_seq,
        )
        .with_status(status_tx.clone())
        .with_subtree(subtree.clone()),
    );

    let marker = context.task_marker();
    let abort_reason = AbortReason::new(status_tx.clone(), marker.clone(), id.clone());
    let join = handle.spawn(MarkedRun::new(
        marker,
        run_actor(
            subtree.clone(),
            status_tx.clone(),
            CaughtDrop::new(actor, |actor, _| end_value(actor), id.clone()),
            CaughtDrop::new(context, end_context, id.clone()),
            CaughtDrop::new(rx, end_queue, id.clone()),
            CaughtDrop::new(priority_rx, end_lane, id.clone()),
            abort_reason,
            system_rx,
            stop_rx,
            turn_clock,
            guard,
            ack,
        ),
    ));

    // Attach the abort handle to the just-created registry entries. Any
    // shutdown sweep that lands in the brief window before this call treats
    // the entry as not-yet-abortable (see `AnyActorHandle::abort`).
    target_system.attach_abort(&id, name.as_deref(), join.abort_handle(), registration_seq);

    Ok((actor_handle, join, status_tx, subtree_watch))
}

/// Spawns the runner task for one child incarnation into the supervisor's
/// death plane.
///
/// The runner owns the child's `JoinHandle<StopReason>`, its armed link
/// guard and its stop ladder. It awaits the task's completion (guaranteed by
/// tokio to happen only after ALL the child's drops, including its
/// `RegistryGuard`, have finished - so the registered name is already free
/// once this resolves), derives the real death reason (normal return, panic,
/// or abort), and writes it into the incarnation's slot cell. Any stop
/// committed for this incarnation, from any origin, is escalated here under
/// the order's policy, so no caller future and no detached timer owns any
/// step of it.
///
/// The runner also carries `guard` for the whole span of its own execution:
/// if this task is ever aborted before the child's task ends - most notably
/// before it is ever polled at all, when the supervisor's own death plane is
/// dropped as part of a Kill teardown - the guard's `Drop` still fires from
/// the abandoned future's own drop glue.
///
/// The task's own result - the child's id, its real stop reason, and its
/// incarnation - is picked up by the run loop via `JoinSet::join_next_with_id`
/// on the supervisor's death plane; no shared channel of any capacity sits
/// between a child's death and its supervisor observing it, so however many
/// children die at once, none of their fates are lost waiting for room.
/// `grace` is the incarnation's kill grace: the time between its `Kill` and
/// its abort. The returned handle identifies the task in the plane.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_runner(
    death_set: &mut JoinSet<DeathOutcome>,
    child_id: ActorId,
    incarnation: u64,
    join: JoinHandle<StopReason>,
    lane: StopLane,
    io: SlotIo,
    guard: ChildLinkGuard,
    grace: Duration,
) -> AbortHandle {
    death_set.spawn(async move {
        let mut guard = guard;
        let mut join = join;
        let mut io = io;
        let reason = run_ladder(&mut join, &lane, &mut io, grace).await;
        io.record_fate(ChildFate {
            reason: reason.clone(),
            incarnation,
        });
        // The child has terminated and its fate is recorded: the guard has
        // nothing left to force, so its drop below must not raise Kill,
        // rewrite the terminal status, or occupy the reaper.
        guard.disarm();
        (child_id, reason, incarnation)
    })
}

// ---------------------------------------------------------------------------
// Actor run loop
// ---------------------------------------------------------------------------

/// Runs the actor's whole lifecycle: init, message loop, teardown.
///
/// `ack` (see [`spawn_actor`]) is resolved from exactly one of two places in
/// this function: the success arm right after the `Running` transition, or,
/// if init failed, the end of the teardown, after `registry_guard` has
/// dropped and the stop lane is sealed, before the queued messages and the
/// actor value are dropped. The timers and streams are released from the
/// mailbox's close on and the forwarders' end is awaited before the seal,
/// so a slow timer or stream `Drop` delays a failed init's answer.
/// That ordering is load-bearing (OTP 26 corpse-consumed parity): it is why
/// a `pre_start` or `on_started` failure breaks out of a labeled block that
/// falls through to the shared teardown tail instead of returning directly.
///
/// On return the end keeps this order: a supervisor's child factories and
/// supervision state drop with its children's teardown, then the actor's
/// name is freed, then everything else it owned is dropped (its queued
/// messages, its value, its context), then `Stopped` is published and its
/// status plane closes, then its subtree ends. The name is free before the
/// queued messages, the value and the context drop; the timers and streams
/// are released from the mailbox's close on (the close comes once
/// `on_stopped` has returned, the initialization failed, or a `Kill` ended
/// the run, which skips `on_stopped`), each when its forwarder next runs,
/// which can come before the name is freed, and the forwarders' end is
/// awaited after it. On an abort while suspended before the mailbox's
/// close, the callback in progress drops first, with the message it
/// handles, then the name is freed, then the queued messages, the context
/// (which ends the timers and streams, each forwarder holding the plane and
/// the subtree open until its future drops) and the value; the plane then
/// closes without `Stopped`, and the subtree ends. On an abort after the
/// close, the timers and streams follow the return path's rule: released
/// from the close on, before or after the name is freed. On an abort
/// before the first poll the value, the context and the queued
/// messages drop while the name is still registered, and the plane reads as
/// ended only after the name is freed. Only the crate's own end signals (the
/// subtree hold, the status senders) outlive what the actor owned, and on
/// every path every drop of what it owned is caught (see [`CaughtDrop`];
/// the callback in progress is caught by `catch_callback`).
/// Each path releases the parameters in this order:
///
/// - Returning: `system_rx` once the message loop is over (or the init
///   failed), then the tail releases `registry_guard` (the name is freed),
///   the forwarders, `stop_rx` after sealing the lane, `ack` (answered for a
///   failed init), `mailbox_slot` and `priority_slot` (the queued messages,
///   one at a time), `actor_slot` and `ctx_slot`, publishes `Stopped`
///   through `status`, and the return drops `turn_clock`, `status` and
///   `_subtree`, last to first. No await follows the seal.
/// - Aborted while suspended, or unwound by this task's own panic: the
///   body's locals, then the parameters last to first, so `ack`,
///   `registry_guard`, `turn_clock`, `stop_rx`, `system_rx`, `_abort_reason`
///   (the reason is written: the name is free, and no queued message has
///   dropped), `priority_slot`, `mailbox_slot`, `ctx_slot`, `actor_slot`,
///   `status`, `_subtree`; whichever of them the tail already released is
///   skipped.
/// - Aborted before the first poll: first to last, so `_subtree`, `status`,
///   `actor_slot`, `ctx_slot` (the context writes the reason), `mailbox_slot`,
///   `priority_slot`, `_abort_reason`, `system_rx`, `stop_rx`, `turn_clock`,
///   `registry_guard`, `ack`. The ack is answered last there, after the
///   queued messages drop: an awaited spawn with a start timeout is bounded
///   by that timeout and the settle wait after its abort.
///
/// The comments on `_subtree`, `status` and `registry_guard` say how each
/// path keeps the order.
#[allow(clippy::too_many_arguments)]
async fn run_actor<A: Actor>(
    // The actor's hold on its subtree token, declared first. On return and
    // on an abort while suspended it is the last parameter dropped, after
    // `status`, so the subtree ends after the status plane. On an abort
    // before the first poll it is the first parameter dropped, while the
    // context and the registry guard still hold the subtree: the guard,
    // declared after everything the actor owns, releases the last hold
    // after it freed the name.
    _subtree: Arc<SubtreeHold>,
    // A sender of the status plane, declared second: the tail publishes
    // `Stopped` through it once everything the actor owned is dropped. On
    // return and on an abort while suspended it is dropped after every other
    // parameter but `_subtree`, so the plane closes after the actor value,
    // the context and the queued messages are gone and the name is free. On
    // an abort before the first poll the registry guard holds the plane open
    // the same way it holds the subtree. Each timer and stream forwarder
    // holds both signals too, so the plane and the subtree also outlive the
    // forwarders' futures.
    status: watch::Sender<StatusValue>,
    // Everything the actor owns, each in a `CaughtDrop`: dropped any way
    // other than by the tail below (an abort, before or after the first
    // poll, or the task's own unwind), its drop still catches every panic,
    // so drop glue never unwinds and the task ends as the abort or the
    // unwind does.
    mut actor_slot: CaughtDrop<A>,
    mut ctx_slot: CaughtDrop<ActorContext<A>>,
    mut mailbox_slot: CaughtDrop<mpsc::Receiver<ActorEnvelope<A>>>,
    mut priority_slot: CaughtDrop<Option<mpsc::Receiver<ActorEnvelope<A>>>>,
    // Declared right after the queues, never a local of the body: on an abort
    // while suspended, or this task's own unwind, the parameters drop last to
    // first, so it writes the abort reason after `ack` and `registry_guard`
    // (the name is free) and before the queued messages, the context and the
    // value drop. A local would drop before `registry_guard`, and a reader of
    // the reason could meet the name still registered. On return the seal
    // wrote the reason first and it writes nothing.
    _abort_reason: AbortReason,
    mut system_rx: mpsc::Receiver<SystemMessage>,
    mut stop_rx: watch::Receiver<LaneState>,
    turn_clock: TurnClock,
    // Declared after everything the actor owns: on an abort before the first
    // poll it frees the name, then releases its status sender and its
    // subtree hold, after the value, the context and the queued messages
    // are dropped.
    registry_guard: Option<RegistryGuard>,
    mut ack: Option<oneshot::Sender<Result<(), ActorError>>>,
) -> StopReason {
    let actor = actor_slot.get_mut();
    let ctx = ctx_slot.get_mut();
    let mailbox = mailbox_slot.get_mut();
    let priority = priority_slot.get_mut();

    // Phase 1 + 2: pre_start/on_started validation (fail-fast gate). A panic
    // is a failed init, exactly like `Err` (self-cleaning contract:
    // on_stopped is not called). The outcome is captured instead of returned
    // directly so a failure still falls through to the shared teardown tail
    // below: the failure ack (if any) must only fire after the registry guard
    // has dropped, so an immediate same-name respawn can never race the dying
    // task and observe NameTaken (OTP 26 corpse-consumed parity).
    //
    // The lane is read (never consumed) before `pre_start` and after it
    // returns `Ok`: a `Kill` pending at either read skips every remaining
    // start hook, records no failure and sends no ack. A `Kill` raised while
    // `on_started` runs is seen by the message loop before any hook.
    let mut killed_in_init = false;
    let init_result: Result<(), ActorError> = 'init: {
        if init_kill_check(stop_rx.borrow().reason.as_ref()) == InitStep::KillNow {
            killed_in_init = true;
            break 'init Ok(());
        }

        // Phase 1: pre_start validation.
        turn_clock.begin();
        let phase1 = match catch_callback(actor.pre_start(ctx)).await {
            Ok(result) => result,
            Err(payload) => Err(ActorError::Panic(payload_into_string(payload))),
        };
        turn_clock.end();
        if let Err(err) = phase1 {
            break 'init Err(err);
        }
        if init_kill_check(stop_rx.borrow().reason.as_ref()) == InitStep::KillNow {
            killed_in_init = true;
            break 'init Ok(());
        }

        // Phase 2: on_started initialization.
        turn_clock.begin();
        let phase2 = match catch_callback(actor.on_started(ctx)).await {
            Ok(result) => result,
            Err(payload) => Err(ActorError::Panic(payload_into_string(payload))),
        };
        turn_clock.end();
        if let Err(err) = phase2 {
            break 'init Err(err);
        }

        Ok(())
    };
    let started = init_result.is_ok() && !killed_in_init;
    // The first panic of a drop before the reason is final that the final
    // collection or the children's teardown meets (a child factory, the
    // supervision state); applied once the children are stopped.
    let mut teardown_panic: Option<String> = None;

    let mut stop_reason = if killed_in_init {
        StopReason::Kill
    } else {
        match &init_result {
            Ok(()) => {
                ctx.set_status(ActorStatus::Running);
                // Success ack: right after the Running transition, before the
                // actor ever touches its mailbox.
                if let Some(tx) = ack.take() {
                    let _ = tx.send(Ok(()));
                }
                StopReason::Graceful
            }
            Err(err) => {
                ctx.record_failure(err.clone());
                StopReason::Failure(err.clone())
            }
        }
    };

    // Phase 3: Message loop - biased select! gives the stop lane top
    // priority, then the system channel, then user messages.
    // Never entered when init failed: the actor must never enter the loop.
    if started {
        // Set once the normal mailbox reports closed, so the loop can keep
        // draining a still-open priority lane instead of exiting early: the
        // two user-facing lanes close independently, and the loop only ever
        // breaks once BOTH have reported closed (the system channel's own
        // close keeps its immediate break, unrelated to either lane).
        let mut mailbox_closed = false;
        loop {
            tokio::select! {
                biased;

                // Stop lane - highest priority, ahead of the system channel
                // and the mailbox alike. Only ever observed here, at the top
                // of the loop between two `handle` invocations, so it never
                // races or cancels an in-flight handler.
                changed = stop_rx.changed() => {
                    match changed {
                        Ok(()) => {
                            let reason = stop_rx.borrow_and_update().reason.clone();
                            let Some(reason) = reason else { continue };
                            // Tier 3 (Kill): bypass ALL callbacks (brutal_kill parity)
                            if matches!(reason, StopReason::Kill) {
                                stop_reason = StopReason::Kill;
                                break;
                            }
                            // Tier 1 (Graceful/ParentRequest): pre_stop gate, vetoable.
                            // A pre_stop panic lets the stop proceed as a Failure.
                            if matches!(reason, StopReason::Graceful | StopReason::ParentRequest) {
                                turn_clock.begin();
                                let pre_stop_result =
                                    catch_callback(actor.pre_stop(&reason, ctx)).await;
                                turn_clock.end();
                                // Read without marking the lane's current version as
                                // seen: only the `select!` arm's own `changed()` may
                                // consume a version, so a same-severity raise that
                                // landed while `pre_stop` was in flight (and is still
                                // unseen here) keeps `changed()` armed and re-fires
                                // this gate on the next iteration instead of being
                                // lost to a veto.
                                //
                                // The lane is monotone (see `StopLane::raise`), so
                                // `latest` is never lower-severity than `reason`: it is
                                // either unchanged or a higher tier raised while
                                // `pre_stop` was in flight. Precedence, in order: a
                                // `Kill` observed here wins over an accept, a veto, or
                                // a panic alike (Kill bypasses every callback); a
                                // `pre_stop` panic is recorded and wins over an accept,
                                // a veto, or a forced reason raised meanwhile (the
                                // panic already happened, and masking it would
                                // silently drop a real failure); otherwise a forced
                                // reason (Failure, ParentKilled or Cancelled) wins over
                                // an accept or a veto, though `on_stopped` still runs
                                // for it.
                                let latest = stop_rx.borrow().reason.clone();
                                if matches!(latest, Some(StopReason::Kill)) {
                                    stop_reason = StopReason::Kill;
                                    break;
                                }
                                match pre_stop_result {
                                    Err(payload) => {
                                        let err =
                                            ActorError::Panic(payload_into_string(payload));
                                        ctx.record_failure(err.clone());
                                        stop_reason = StopReason::Failure(err);
                                        break;
                                    }
                                    Ok(accepted) => match latest {
                                        Some(r) if StopSeverity::of(&r) == StopSeverity::Forced => {
                                            stop_reason = r;
                                            break;
                                        }
                                        _ if accepted => {
                                            stop_reason = reason;
                                            break;
                                        }
                                        _ => continue, // Actor rejected the stop; a
                                            // later same-severity raise bumps the
                                            // generation and re-fires this gate.
                                    },
                                }
                            }
                            // Tier 2 (Failure/ParentKilled/Cancelled): non-vetoable, on_stopped still runs
                            stop_reason = reason;
                            break;
                        }
                        Err(_) => break, // stop lane closed: no sender can ever reach us
                    }
                }

                // System channel - priority over user messages
                sys_msg = system_rx.recv() => {
                    match sys_msg {
                        Some(SystemMessage::GetStatus(reply_tx)) => {
                            let info = build_status_info(ctx);
                            let _ = reply_tx.send(info);
                        }
                        None => break, // system channel closed
                    }
                }

                // Supervision restart + death planes - combined into one
                // `select!` branch because `select!` constructs every
                // branch's future eagerly, so two separate `&mut ctx`
                // borrows across two branches can never coexist; the restart
                // plane is still drained first, biased, within this combined
                // branch. Pends forever with no supervision config or nothing
                // currently in flight on either plane, so this never becomes
                // a busy poll.
                event = next_supervision_event(ctx) => {
                    match event {
                        Some(SupervisionEvent::Restart(Ok((task, outcome)))) => {
                            if let Some(sup) = ctx.supervision_mut() {
                                sup.task_owner.remove(&task);
                            }
                            match handle_restart_outcome(actor, ctx, outcome).await {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(reason) => {
                                    stop_reason = reason;
                                    break;
                                }
                            }
                        }
                        Some(SupervisionEvent::Restart(Err(err))) => {
                            // The restart task itself failed before returning
                            // its outcome. Any `ChildLinkGuard` it had
                            // already armed still ran its own drop glue as
                            // part of the abandoned future's teardown. The
                            // attempt's end is still accounted for: followed
                            // to its incarnation's end when one was spawned,
                            // reported at once as a failed attempt when none
                            // was.
                            if let (Some(outcome), _) = ctx.lost_restart_task(err) {
                                match handle_restart_outcome(actor, ctx, outcome).await {
                                    ControlFlow::Continue(()) => {}
                                    ControlFlow::Break(reason) => {
                                        stop_reason = reason;
                                        break;
                                    }
                                }
                            }
                        }
                        Some(SupervisionEvent::Death(Ok((task, (child_id, child_reason, incarnation))))) => {
                            if let Some(sup) = ctx.supervision_mut() {
                                sup.task_owner.remove(&task);
                            }
                            let event = ChildStoppedInternal { child_id, reason: child_reason, incarnation };
                            match handle_child_stopped(actor, ctx, event).await {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(reason) => {
                                    stop_reason = reason;
                                    break;
                                }
                            }
                        }
                        Some(SupervisionEvent::Death(Err(err))) => {
                            // The runner task itself failed before
                            // completing (see `ChildLinkGuard`); its own drop
                            // glue already signalled and scheduled the
                            // child. The incarnation's end is still reported
                            // once it ended, through a replacement task.
                            let _ = ctx.lost_death_task(err);
                        }
                        Some(SupervisionEvent::Verdict) => {
                            // A head that received the Unresponsive verdict is
                            // settled: the queue goes on to the next member.
                            if let Some(sup) = ctx.supervision_mut() {
                                advance_stop_queue(sup);
                            }
                        }
                        Some(SupervisionEvent::Forwarder(outcome)) => {
                            // A fired one-shot, a finished stream, or a
                            // forwarder that panicked - reaping it here frees
                            // its registration and, on a panic, records it as
                            // the last forwarder error and logs it once (with
                            // the `tracing` feature). Never stops the actor.
                            ctx.reap_forwarder(outcome);
                        }
                        None => {} // unreachable: `next_supervision_event` never resolves to `None`
                    }
                    // A pending group restart that can make progress only
                    // through unresponsive members ends the supervisor: its
                    // own parent decides.
                    if let Some(reason) = check_verdicts(ctx) {
                        stop_reason = reason;
                        break;
                    }
                }

                // Priority lane - received ahead of the normal mailbox,
                // behind every supervision plane above it. Guarded so a
                // disabled lane (no `with_priority_lane` at spawn) is never
                // even polled, and `recv_optional`'s own `pending()` arm is a
                // second line of defense against ever busy-polling one.
                // Closing independently of the mailbox: once this lane
                // itself is drained and closed, the loop keeps draining the
                // mailbox until IT also reports closed (see the mailbox arm
                // below).
                envelope = recv_optional(priority), if priority.is_some() => {
                    match envelope {
                        Some(env) => {
                            match dispatch(actor, ctx, env, &turn_clock).await {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(reason) => {
                                    stop_reason = reason;
                                    break;
                                }
                            }
                        }
                        None => {
                            // The priority lane itself is closed: stop
                            // polling it, and only exit the loop here if the
                            // mailbox already closed too - otherwise the
                            // mailbox arm below is still the reason to keep
                            // running.
                            *priority = None;
                            if mailbox_closed {
                                break;
                            }
                        }
                    }
                }

                // User mailbox - disabled once it has already reported
                // closed, so a still-open priority lane keeps being drained
                // instead of this arm firing again on every iteration.
                envelope = mailbox.recv(), if !mailbox_closed => {
                    match envelope {
                        Some(env) => {
                            match dispatch(actor, ctx, env, &turn_clock).await {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(reason) => {
                                    stop_reason = reason;
                                    break;
                                }
                            }
                        }
                        None => {
                            // Mailbox closed (all handles dropped). Exit only
                            // if the priority lane is also gone (or never
                            // existed); otherwise it alone still justifies
                            // running.
                            if priority.is_none() {
                                break;
                            }
                            mailbox_closed = true;
                        }
                    }
                }
            }
        }

        // The final deliveries, then Phase 4: on_stopped (terminate/2 parity:
        // runs on failures and panics, skipped only for Kill / brutal_kill).
        // A supervisor first delivers, once each, the child deaths still
        // pending when its message loop ended (rule: its status reads
        // `Stopping` in every one of them). A `Kill` that arrives meanwhile
        // ends the deliveries and skips `on_stopped`.
        let mut pending_exit: Option<StopReason> = None;
        if !matches!(stop_reason, StopReason::Kill) {
            ctx.set_status(ActorStatus::Stopping);
            // A task woken by that publish is queued on this worker, and a
            // callback that blocks the thread would starve it until the
            // callback returns. One yield lets it run first, before the first
            // callback of the stop: after the final collection for a
            // supervisor, whose collection must see the deaths of the turn
            // that ended its loop.
            if ctx.supervision_ref().is_some() {
                match final_drain(actor, ctx, &stop_rx, &stop_reason, &mut teardown_panic).await {
                    FinalOutcome::Killed => stop_reason = StopReason::Kill,
                    FinalOutcome::Done { pending } => pending_exit = pending,
                }
            } else {
                tokio::task::yield_now().await;
            }
        }
        // The loop can end for a reason that read no lane (a handler's `Err`
        // or panic, an `Err` from a child report, an exhausted budget, a
        // closed channel): a `Kill` stored by now still skips `on_stopped`,
        // so no hook of the target begins after the one that was running
        // returns. For a supervisor that delivered reports this is the read
        // that closes the deliveries.
        if !matches!(stop_reason, StopReason::Kill) && kill_stored(&stop_rx) {
            stop_reason = StopReason::Kill;
        }
        if !matches!(stop_reason, StopReason::Kill) {
            turn_clock.begin();
            let stopped = catch_callback(actor.on_stopped(&stop_reason, ctx)).await;
            turn_clock.end();
            // A failed final delivery makes the exit reason a failure once
            // `on_stopped` has returned; `on_stopped`'s own result is applied
            // after it, the same way.
            if let Some(pending) = pending_exit {
                stop_reason = pending;
            }
            match stopped {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    stop_reason = StopReason::Failure(err);
                }
                Err(payload) => {
                    let panic_err = ActorError::Panic(payload_into_string(payload));
                    ctx.record_failure(panic_err.clone());
                    // A cleanup panic must not mask the original failure.
                    if !matches!(stop_reason, StopReason::Failure(_)) {
                        stop_reason = StopReason::Failure(panic_err);
                    }
                }
            }
        }
    } // started: Phase 3 (loop) + Phase 4 (on_stopped)

    // The last read before the lane narrows: a `Kill` raised while
    // `on_stopped` ran, or during a failed init, is the final reason. The
    // reason `on_stopped` already received is never rewritten, and the read
    // never consumes the lane's version.
    if !matches!(stop_reason, StopReason::Kill) && kill_stored(&stop_rx) {
        stop_reason = StopReason::Kill;
    }

    // Close the system receiver BEFORE stopping children, and narrow the stop
    // lane to `Kill`: late system senders (escalation timers, new get_status
    // callers) fail at once instead of parking, a get_status already queued
    // fails with the actor's reason once that reason is final at the seal,
    // and from this exact point `ActorHandle::stop`/`ActorSystem::stop`
    // answer `SendError::Closed` for every reason but `Kill`. A `Kill` is still accepted: the teardown
    // below observes it and turns the rest of itself into the `Kill` path,
    // and the lane seals only just before the queued messages and the actor
    // value are dropped. The receiver stays alive until then.
    drop(system_rx);
    ctx.self_handle().stop_lane().close_to_non_kill();

    // Close (not drop - the receivers stay alive for the explicit drop
    // further down) the two user lanes at this exact same point: every
    // sender's `is_closed()` becomes true and every `send`/`try_send`
    // against it starts returning `Closed`, immediately, from here through
    // the rest of this actor's teardown (the child cascade below included).
    // Without this, `notify`/`try_notify`/`is_alive` would keep reporting
    // this actor as reachable for as long as the child cascade takes, even
    // though the stop lane and system channel already agree it is not -
    // and any message accepted in that window is simply discarded, never
    // handled, once this task returns.
    mailbox.close();
    if let Some(priority) = priority.as_mut() {
        priority.close();
    }

    // Phase 5: Stop all children (reverse start order, a pool all at once,
    // honoring Shutdown) -
    // except when this task is itself dying with Kill: that reason never
    // awaits its own children. Its supervision state - the death plane
    // included - is dropped directly instead, which aborts every live
    // child's runner task; each one's link guard fires from its own drop
    // glue, raising Kill on that child's lane and repeating the same
    // inversion one level down, without this task ever awaiting the subtree
    // it is discarding.
    let actor_id = ctx.actor_id().clone();
    if !matches!(stop_reason, StopReason::Kill)
        && stop_all_children(ctx, &mut stop_rx, &mut teardown_panic).await == Teardown::Killed
    {
        stop_reason = StopReason::Kill;
    }
    if matches!(stop_reason, StopReason::Kill) {
        if let Some(sup) = ctx.take_supervision() {
            keep_first_of(
                &mut teardown_panic,
                end_supervision(sup, &actor_id),
                &actor_id,
            );
        }
    }
    if let Some(payload) = teardown_panic {
        end_drop_panicked(&mut stop_reason, payload, &actor_id);
    }

    // RegistryGuard::drop unregisters the actor: the registered name is free
    // from here, before the queued messages, the value and the context are
    // dropped and before its status plane reads as ended. The timers and
    // streams are released from the mailbox's close on, which can come
    // before this point. A caller that wakes on `Stopped` (or on a
    // closed plane) can reuse the name at once, and tokio completes this
    // task's JoinHandle only after every drop, so the name is free before
    // the parent acts on the death either way.
    drop(registry_guard);

    // Every timer and stream forwarder is ended and awaited first, each
    // future dropped with the stream, message or factory it owns; a panic in
    // the drop of a timer or stream still registered is part of the end.
    // From the mailbox's close (above) on, every forwarder releases what it
    // owns when it next runs, and this abort drops what one that has not run
    // still owns; either way a drop panic is classified by registration. A
    // panic of a poll, a drop made before the forwarder saw the close, and
    // any panic of a timer or stream cancelled during the run belong to the
    // run (an aborted forwarder is never polled again), whichever task runs
    // first.
    // A `Kill` raised meanwhile is still accepted and read by the seal below. This is
    // the last await of the task: from the seal on, the end runs without a
    // yield, so an abort that lands while the queued messages or the value
    // drop cannot change how the task ends.
    if let Some(payload) = ctx.end_forwarders().await {
        end_drop_panicked(&mut stop_reason, payload, &actor_id);
    }

    // Seal the lane: every `Kill` a caller was told was accepted was either
    // observed above or is read here, and every request from here on is
    // refused with `Closed`, just before the queued messages and the actor
    // value are dropped. The receiver is dropped with it.
    if let Some(StopReason::Kill) = ctx.self_handle().stop_lane().seal() {
        stop_reason = StopReason::Kill;
    }
    drop(stop_rx);

    // The reason is final here, written into the status plane once: a panic
    // in a forwarder's drop at the end (see above) already made any reason
    // but a `Kill` or a `Failure` a failure, and the seal's `Kill` is read.
    // Readers of the plane (a request whose reply is dropped, `wait_stopped`,
    // a stop report) read this reason, as the task's return does.
    write_final_reason(&status, &stop_reason);

    // The init ack resolves here, after the seal and before the queued
    // messages and the value are dropped: however long one of their `Drop`s
    // takes, a failed init is reported as its error, a `Kill` its caller
    // raises on reading the error is refused and leaves that error the
    // reason, and a same-name respawn from the caller never observes
    // NameTaken (OTP 26 corpse-consumed parity). An init a `Kill` cut drops
    // the ack unanswered. So does a failed init whose end a `Kill` reached
    // before the seal: the final reason is `Kill`, the awaited spawn reads
    // `Killed` from the plane, one answer, and the init's error is logged
    // once with the actor's id.
    if let Some(tx) = ack.take() {
        if let Err(err) = init_result {
            if matches!(stop_reason, StopReason::Kill) {
                #[cfg(feature = "tracing")]
                caught_log!(warn,
                    actor_id = %actor_id,
                    error = %err,
                    "the actor's initialization failed and a Kill reached its end before the reason was final; the spawn reads Killed"
                );
                #[cfg(not(feature = "tracing"))]
                let _ = err;
                drop(tx);
            } else {
                let _ = tx.send(Err(err));
            }
        }
    }

    // The queued messages are dropped one at a time, each drop caught; the
    // mailbox and the priority lane were closed when the message loop ended.
    // Then the actor value and the context, which holds nothing of the
    // actor's own by then. A panic in any of these drops is caught and
    // changes nothing: the reason is final, the end goes on, `Stopped` is
    // still published, and with the `tracing` feature each panic is logged
    // once with the actor's id: the first below, a later one where it is
    // discarded.
    let mut late_panic = mailbox_slot.end();
    keep_first_of(&mut late_panic, priority_slot.end(), &actor_id);
    keep_first_of(&mut late_panic, actor_slot.end(), &actor_id);
    keep_first_of(&mut late_panic, ctx_slot.end(), &actor_id);
    #[cfg(feature = "tracing")]
    if let Some(payload) = &late_panic {
        caught_log!(warn,
            actor_id = %actor_id,
            reason = %stop_reason,
            panic = %payload,
            "a drop after the actor's reason was final panicked; the reason stands"
        );
    }
    #[cfg(not(feature = "tracing"))]
    let _ = late_panic;
    publish_stopped(&status);

    #[cfg(feature = "tracing")]
    caught_log!(info,
        actor_id = %actor_id,
        reason = %stop_reason,
        "Actor stopped"
    );

    stop_reason
}

/// The reason, `Failure(Panic(UNCAUGHT_PANIC))`, written for a task whose own
/// poll unwound past every catch: every call of user code is caught, so only
/// the crate's own code unwinds there. The task ends in the order of an abort
/// while suspended, with this reason instead of `Kill`: its status plane, the
/// runner or restart attempt that joins it, and an awaited spawn whose
/// initialization the unwind cut (as `SpawnError::Init`) read it. The
/// payload is logged by whoever joins the task.
pub(crate) const UNCAUGHT_PANIC: &str = "the actor's task panicked outside every catch";

/// Set while this actor task's own future is being polled, and cleared after
/// each poll, an unwind included ([`MarkedRun`]). One per task: one thread
/// may poll or drop another actor's task inside a callback. A drop of the
/// task's state that writes the reason reads it to tell this task's own
/// unwind, inside its poll, from an abort, a shutdown, or a runtime dropped
/// during an unrelated unwind, which drop the task outside its poll.
#[derive(Clone, Default)]
pub(crate) struct TaskMarker(Arc<AtomicBool>);

impl TaskMarker {
    /// The reason of a task dropped before its reason was written: a panic
    /// when this task's own poll is unwinding, `Kill` otherwise.
    pub(crate) fn abort_reason(&self) -> StopReason {
        if self.0.load(Ordering::SeqCst) && std::thread::panicking() {
            StopReason::Failure(ActorError::Panic(UNCAUGHT_PANIC.to_string()))
        } else {
            StopReason::Kill
        }
    }
}

/// The actor task's future: sets the task's marker before each poll of
/// `run_actor` and clears it after, through a guard that also clears it on
/// an unwind. On this task's own panic, everything inside the future drops
/// during the unwind of the inner poll, while the marker is still set.
struct MarkedRun<F> {
    marker: TaskMarker,
    inner: Pin<Box<F>>,
}

impl<F: Future> MarkedRun<F> {
    fn new(marker: TaskMarker, inner: F) -> Self {
        Self {
            marker,
            inner: Box::pin(inner),
        }
    }
}

/// Clears a task's marker when it drops.
struct ClearMarker<'a>(&'a AtomicBool);

impl Drop for ClearMarker<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl<F: Future> Future for MarkedRun<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        this.marker.0.store(true, Ordering::SeqCst);
        let _clear = ClearMarker(&this.marker.0);
        this.inner.as_mut().poll(cx)
    }
}

/// Writes the abort reason into the status plane when it drops and no reason
/// is written: `Kill`, or a panic for this task's own unwind
/// ([`TaskMarker::abort_reason`]). See `run_actor`'s `_abort_reason`.
pub(crate) struct AbortReason {
    status: watch::Sender<StatusValue>,
    marker: TaskMarker,
    /// The actor, for the hook tests observe the write through.
    #[cfg_attr(not(test), allow(dead_code))]
    owner: ActorId,
}

impl AbortReason {
    fn new(status: watch::Sender<StatusValue>, marker: TaskMarker, owner: ActorId) -> Self {
        Self {
            status,
            marker,
            owner,
        }
    }
}

impl Drop for AbortReason {
    fn drop(&mut self) {
        if self.status.borrow().reason.is_some() {
            return;
        }
        #[cfg(test)]
        end_hooks::run(end_hooks::REASON, &self.owner);
        StatusValue::write_reason(&self.status, self.marker.abort_reason(), false);
    }
}

/// Hooks tests attach by point and actor id: [`REASON`](end_hooks::REASON)
/// runs just before an abort guard writes its reason,
/// [`NAME`](end_hooks::NAME) just before a registry guard frees the name,
/// [`NAME_FREED`](end_hooks::NAME_FREED) just after it freed the name, and
/// [`WALK`](end_hooks::WALK) in a supervisor's teardown, before it orders a
/// child that is still in its registry to stop (keyed by the supervisor).
#[cfg(test)]
pub(crate) mod end_hooks {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use crate::types::ActorId;

    pub(crate) const REASON: &str = "reason";
    pub(crate) const NAME: &str = "name";
    pub(crate) const NAME_FREED: &str = "name freed";
    pub(crate) const WALK: &str = "walk";

    type Hook = Box<dyn Fn() + Send + Sync>;

    fn hooks() -> &'static Mutex<HashMap<(&'static str, ActorId), Hook>> {
        static HOOKS: OnceLock<Mutex<HashMap<(&'static str, ActorId), Hook>>> = OnceLock::new();
        HOOKS.get_or_init(Default::default)
    }

    /// Runs `hook` once, at `point` of the actor `id`.
    pub(crate) fn attach(
        point: &'static str,
        id: ActorId,
        hook: impl Fn() + Send + Sync + 'static,
    ) {
        hooks().lock().unwrap().insert((point, id), Box::new(hook));
    }

    pub(crate) fn run(point: &'static str, id: &ActorId) {
        let hook = hooks().lock().unwrap().remove(&(point, id.clone()));
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Runs `drop` and catches a panic it raises.
fn catch_drop(drop: impl FnOnce()) -> Result<(), String> {
    catch_sync(drop).map_err(payload_into_string)
}

/// Keeps the first of two panics of one step of the actor `owner`'s end. A
/// later one is discarded, and with the `tracing` feature it is logged,
/// naming the actor, so every panic leaves a record.
pub(crate) fn keep_first_of(first: &mut Option<String>, next: Option<String>, owner: &ActorId) {
    match (first.is_some(), next) {
        (false, next) => *first = next,
        (true, Some(discarded)) => log_discarded(&discarded, owner),
        (true, None) => {}
    }
}

/// Logs, with the `tracing` feature, a panic a step discards because it
/// keeps an earlier one.
pub(crate) fn log_discarded(payload: &str, owner: &ActorId) {
    #[cfg(feature = "tracing")]
    caught_log!(warn,
        actor_id = %owner,
        panic = %payload,
        "a drop panicked after an earlier panic of the same step was kept; this panic is discarded"
    );
    #[cfg(not(feature = "tracing"))]
    let _ = (payload, owner);
}

/// Something the actor owns, held until the end of its task drops it with
/// [`end`](Self::end). However else it is dropped (an abort before the first
/// poll or while suspended, or the task's own unwind), its drop goes through
/// the same `end`, which catches every panic, so drop glue never unwinds;
/// with the `tracing` feature such a panic is logged once with the actor's
/// id, and the task ends as the abort (`Kill`) or the unwind
/// (`Failure(Panic(..))`) does.
pub(crate) struct CaughtDrop<T> {
    value: Option<T>,
    end: fn(T, &ActorId) -> Option<String>,
    /// The actor that owns the value, passed to `end` on every path (the
    /// end of the task, an abort or an unwind) so each log line names it.
    owner: ActorId,
}

impl<T> CaughtDrop<T> {
    pub(crate) fn new(value: T, end: fn(T, &ActorId) -> Option<String>, owner: ActorId) -> Self {
        Self {
            value: Some(value),
            end,
            owner,
        }
    }

    fn get_mut(&mut self) -> &mut T {
        self.value
            .as_mut()
            .expect("the value is held until its end")
    }

    /// Drops the value now; returns the first panic its drop raised.
    fn end(&mut self) -> Option<String> {
        let end = self.end;
        self.value.take().and_then(|value| end(value, &self.owner))
    }
}

impl<T> Drop for CaughtDrop<T> {
    fn drop(&mut self) {
        let panic = self.end();
        #[cfg(feature = "tracing")]
        if let Some(payload) = &panic {
            caught_log!(warn,
                actor_id = %self.owner,
                panic = %payload,
                "a drop at the abort or the unwind of the actor's task panicked; the task ends as the abort or the unwind does"
            );
        }
        #[cfg(not(feature = "tracing"))]
        let _ = (panic, &self.owner);
    }
}

/// Drops a value, catching a panic its drop raises.
fn end_value<T>(value: T) -> Option<String> {
    catch_drop(move || drop(value)).err()
}

/// Closes a queue and drops the messages still in it one at a time, each
/// drop caught, so a second panicking message never panics inside the
/// unwinding of the first.
fn end_queue<T>(mut rx: mpsc::Receiver<T>, owner: &ActorId) -> Option<String> {
    rx.close();
    let mut first = None;
    while let Ok(message) = rx.try_recv() {
        keep_first_of(&mut first, end_value(message), owner);
    }
    keep_first_of(&mut first, end_value(rx), owner);
    first
}

/// [`end_queue`] for the priority lane, which only some actors have.
fn end_lane<T>(rx: Option<mpsc::Receiver<T>>, owner: &ActorId) -> Option<String> {
    rx.and_then(|rx| end_queue(rx, owner))
}

/// Drops a supervisor's state: its child factories one at a time, each drop
/// caught, then the rest.
fn end_supervision(mut sup: SupervisionState, owner: &ActorId) -> Option<String> {
    let mut first = None;
    for (_, factory) in std::mem::take(&mut sup.restart_fns) {
        keep_first_of(&mut first, end_value(factory), owner);
    }
    keep_first_of(&mut first, end_value(sup), owner);
    first
}

/// Drops an actor's context: its supervision state first, as
/// [`end_supervision`] does, then the rest, each drop caught.
fn end_context<A: Actor>(mut ctx: ActorContext<A>, owner: &ActorId) -> Option<String> {
    let mut first = ctx
        .take_supervision()
        .and_then(|sup| end_supervision(sup, owner));
    keep_first_of(&mut first, end_value(ctx), owner);
    first
}

/// Writes the actor's final reason into its status plane, once: a plane
/// that already carries a reason keeps it.
fn write_final_reason(status: &watch::Sender<StatusValue>, reason: &StopReason) {
    StatusValue::write_reason(status, reason.clone(), false);
}

/// Publishes `Stopped` on a plane whose reason is already written.
pub(crate) fn publish_stopped(status: &watch::Sender<StatusValue>) {
    status.send_if_modified(|value| {
        value.status = ActorStatus::Stopped;
        true
    });
}

/// Applies a panic raised by a drop before the reason is final (a registered
/// timer's or stream's item at the forwarders' end, see
/// `ActorContext::end_forwarders`, a child factory or the supervision state
/// at the teardown): any reason but an earlier `Kill` or `Failure` becomes the
/// panic. A panic that leaves an earlier `Kill` or `Failure` as the reason is
/// logged once with the actor's id, as a drop after the seal is.
fn end_drop_panicked(stop_reason: &mut StopReason, payload: String, actor_id: &ActorId) {
    if !matches!(stop_reason, StopReason::Kill | StopReason::Failure(_)) {
        *stop_reason = StopReason::Failure(ActorError::Panic(payload));
        return;
    }
    #[cfg(feature = "tracing")]
    caught_log!(warn,
        actor_id = %actor_id,
        reason = %stop_reason,
        panic = %payload,
        "a drop before the actor's reason was final panicked; the reason stands"
    );
    #[cfg(not(feature = "tracing"))]
    let _ = (payload, actor_id);
}

/// One event from either of the supervisor's two `JoinSet` planes, as
/// returned by [`next_supervision_event`].
enum SupervisionEvent {
    /// A restart attempt resolved (see the restart plane, `restart_set`).
    Restart(Result<(tokio::task::Id, RestartOutcome), JoinError>),
    /// A child runner completed (see the death plane, `death_set`).
    Death(Result<(tokio::task::Id, DeathOutcome), JoinError>),
    /// An incarnation received the Unresponsive verdict.
    Verdict,
    /// A timer or stream forwarder task completed (see the forwarder plane).
    /// Present on every actor, supervisor or not.
    Forwarder(Result<(tokio::task::Id, ForwarderEnd), JoinError>),
}

/// Polls the supervisor's restart and death planes together with every
/// actor's forwarder plane, biased in that order, with the verdict signal
/// between the death plane and the forwarder plane: a death already in the
/// plane settles its member before a verdict is re-checked. All of them live
/// behind the same `&mut ctx` borrow because `select!` constructs every
/// branch's future eagerly - disjoint fields reachable from one borrow can
/// share it, but two separate top-level `select!` branches each borrowing
/// `ctx` themselves cannot. Pends forever with no supervision config and
/// nothing currently in flight on any plane, so this is safe to use as a
/// `select!` branch without ever turning into a busy poll.
async fn next_supervision_event<A: Actor>(ctx: &mut ActorContext<A>) -> Option<SupervisionEvent> {
    let (planes, forwarders) = ctx.supervision_planes_mut();
    let forwarders_pending = !forwarders.is_empty();
    match planes {
        Some((restart_set, death_set, verdicts)) => {
            let restart_pending = !restart_set.is_empty();
            let death_pending = !death_set.is_empty();
            if !restart_pending && !death_pending && !forwarders_pending {
                return std::future::pending::<Option<SupervisionEvent>>().await;
            }
            tokio::select! {
                biased;
                r = restart_set.join_next_with_id(), if restart_pending => {
                    r.map(SupervisionEvent::Restart)
                }
                d = death_set.join_next_with_id(), if death_pending => {
                    d.map(SupervisionEvent::Death)
                }
                _ = verdicts.notified() => Some(SupervisionEvent::Verdict),
                f = forwarders.join_next_with_id(), if forwarders_pending => {
                    f.map(SupervisionEvent::Forwarder)
                }
            }
        }
        None => {
            if !forwarders_pending {
                return std::future::pending::<Option<SupervisionEvent>>().await;
            }
            forwarders
                .join_next_with_id()
                .await
                .map(SupervisionEvent::Forwarder)
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch (user messages only - Stop travels on the stop lane)
// ---------------------------------------------------------------------------

async fn dispatch<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    envelope: ActorEnvelope<A>,
    turn_clock: &TurnClock,
) -> ControlFlow<StopReason> {
    match envelope {
        Envelope::Message { payload, responder } => {
            // `begin` and `end` bracket the callback exactly, on every
            // outcome (normal return, an `Err`, or a caught panic), since
            // `catch_callback` always completes normally here rather than
            // unwinding through this function.
            turn_clock.begin();
            let outcome = catch_callback(actor.handle(payload, ctx)).await;
            turn_clock.end();
            match outcome {
                Ok(Ok(response)) => {
                    // A response nobody receives (a notify, or a send whose
                    // caller is gone) is dropped here, and its drop is user
                    // code: caught, its panic is a panic of the turn.
                    let undelivered = match responder {
                        Some(tx) => tx.send(Ok(response)).err(),
                        None => Some(Ok(response)),
                    };
                    match undelivered.and_then(end_value) {
                        None => ControlFlow::Continue(()),
                        Some(payload) => {
                            let err = ActorError::Panic(payload);
                            ctx.record_failure(err.clone());
                            ControlFlow::Break(StopReason::Failure(err))
                        }
                    }
                }
                Ok(Err(err)) => {
                    // Cast-exception parity: an `Err` from `handle` stops
                    // the actor identically on the notify and send paths. The
                    // send path additionally has a caller to notify, so that
                    // delivery happens first when a responder is present.
                    if let Some(tx) = responder {
                        let _ = tx.send(Err(err.clone()));
                    }
                    ctx.record_failure(err.clone());
                    ControlFlow::Break(StopReason::Failure(err))
                }
                Err(payload) => {
                    // A panic crashes the actor on BOTH paths exactly like a
                    // returned `Err` does (cast-exception parity, same as the
                    // arm above): the only difference is the panic payload
                    // has to be caught and wrapped here instead of being
                    // handed back verbatim. A send() caller still receives it
                    // as a matchable error before the actor stops.
                    let err = ActorError::Panic(payload_into_string(payload));
                    if let Some(tx) = responder {
                        let _ = tx.send(Err(err.clone()));
                    }
                    ctx.record_failure(err.clone());
                    ControlFlow::Break(StopReason::Failure(err))
                }
            }
        }
    }
}

/// Awaits `rx`'s next value when it is `Some`, or pends forever when it is
/// `None`. Used as a `select!` branch for the priority lane: paired with an
/// `if priority.is_some()` precondition on that branch (so a disabled lane
/// is never even polled), the `pending()` arm here is a second line of
/// defense against the branch ever turning into a busy poll.
async fn recv_optional<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

// ---------------------------------------------------------------------------
// Child supervision
// ---------------------------------------------------------------------------

/// What the run loop does with one report of a child's end, once the ledger
/// has recorded it.
enum Routed {
    /// The report waits behind the pending group restart; the ledger records
    /// the decision it owes. Nothing is delivered now.
    Queued,
    /// The report is delivered with this action and decides nothing else.
    Deliver(SupervisionAction),
    /// The restart chain's own start failed: the whole group is retried,
    /// charged to the budget once.
    ChainRetry,
    /// The ordinary death path: the strategy decides.
    Evaluate,
}

/// What the run loop does after it recorded a report as a member's end.
#[derive(Default)]
struct GroupSteps {
    /// The stop phase's barrier emptied: the chain starts with this restart
    /// order, these overrides and the group's scope.
    begin_chain: Option<ChainStart>,
    /// The chain's front member is down: the chain advances.
    advance_chain: bool,
    /// The member leaves the group removed instead of restarted: a
    /// `terminate_child` released it, or it is a `Temporary` member. Its
    /// spec is pruned when it is prunable and its end is delivered as
    /// removed.
    manual_override: bool,
}

/// Records one report of a child's end in the ledger and decides its route.
///
/// The report is of a child's occupant or of its owed end (a stale report
/// was dropped before). Membership is read before any change, and every
/// ledger write names the reported incarnation: a failed restart attempt
/// `Restarting { n, m }` is reported as `m` and recorded as `Down { m, .. }`.
fn route_report(
    sup: &mut SupervisionState,
    ev: &ChildStoppedInternal,
    steps: &mut GroupSteps,
) -> Routed {
    let id = &ev.child_id;
    let incarnation = ev.incarnation;
    let Some((life, origin)) = sup
        .registry
        .get(id)
        .map(|child| (child.lifecycle, child.origin))
    else {
        return Routed::Evaluate;
    };
    let attempt = matches!(life, ChildLifecycle::Restarting { next, .. } if next == incarnation);
    let membership = sup.group_membership(id);
    // What a group does with this member's end, wherever it is delivered.
    let pruned = sup.registry.get(id).is_some_and(|child| {
        rules::group_member_end(child.spec.restart_type) == rules::MemberEnd::PruneAndRemove
    });
    let chain_front = match sup.pending_group.as_ref() {
        Some(GroupPhase::Restarting { queue, started, .. }) if queue.front() == Some(id) => {
            Some(started.get(id).copied())
        }
        _ => None,
    };
    let record = |sup: &mut SupervisionState, pending: DownPending| {
        if let Some(child) = sup.registry.get_mut(id) {
            let _ = child.transition(ChildLifecycle::Down {
                incarnation,
                pending,
            });
        }
    };

    if attempt {
        // The attempt spawned an incarnation if its cell holds the subtree:
        // the next start for the child waits for it, whether or not the
        // incarnation was adopted.
        if let Some(child) = sup.registry.get_mut(id) {
            child.note_subtree();
        }
        let chain_front_own = chain_front == Some(Some(incarnation));
        return match rules::attempt_failure_route(chain_front_own, membership, origin) {
            Route::ChainRetry => {
                record(sup, DownPending::None);
                Routed::ChainRetry
            }
            Route::Member => {
                record(sup, DownPending::None);
                if settle_barrier(sup, id, incarnation) {
                    steps.begin_chain = advance_group_stop(sup);
                }
                steps.advance_chain = chain_front.is_some();
                steps.manual_override = pruned;
                Routed::Deliver(SupervisionAction::RestartInitiated)
            }
            Route::Removed => {
                record(sup, DownPending::None);
                Routed::Deliver(SupervisionAction::Removed)
            }
            Route::Evaluate if sup.pending_group.is_some() => {
                record(sup, DownPending::Evaluate);
                sup.queued_triggers.push_back(ev.clone());
                Routed::Queued
            }
            Route::Evaluate => {
                record(sup, DownPending::None);
                Routed::Evaluate
            }
        };
    }

    if sup.pending_group.is_none() {
        record(sup, DownPending::None);
        return Routed::Evaluate;
    }
    // The stop phase awaits this very incarnation: its end settles the
    // barrier entry, and a member's end belongs to its group.
    let settled = match sup.pending_group.as_mut() {
        Some(GroupPhase::Stopping(group)) => group
            .settle_awaited(id, incarnation)
            .then(|| group.manual_overrides.get(id) == Some(&incarnation)),
        _ => None,
    };
    if let Some(overridden) = settled {
        steps.manual_override = overridden || pruned;
        record(sup, DownPending::None);
        steps.begin_chain = advance_group_stop(sup);
        return Routed::Deliver(SupervisionAction::RestartInitiated);
    }
    if membership == GroupMembership::Queued {
        // A member's own end belongs to its group, whether it owed its
        // decision or died as the occupant (one the chain stopped): the
        // group restarts the member, nothing is charged, and the chain goes
        // on when the member is its front.
        record(sup, DownPending::None);
        steps.advance_chain = chain_front.is_some();
        steps.manual_override = pruned;
        return Routed::Deliver(SupervisionAction::RestartInitiated);
    }
    // A child the group does not restart: its decision waits for the group,
    // and the ledger says so now.
    record(sup, DownPending::Evaluate);
    sup.queued_triggers.push_back(ev.clone());
    Routed::Queued
}

/// What the restart chain of a group starts with: the restart order, the
/// members a `terminate_child` released, and the sets the group keeps through
/// its retries.
struct ChainStart {
    order: Vec<ActorId>,
    overrides: HashMap<ActorId, u64>,
    scope: GroupScope,
}

/// Takes the stop phase's group out of the pending state when its barrier
/// emptied, as the restart order and overrides the chain starts with. A
/// member a `terminate_child` released left the restart order when the call
/// was made; the filter by `overrides` is a second line of defense for it.
fn take_chain_order(sup: &mut SupervisionState) -> Option<ChainStart> {
    match sup.pending_group.take() {
        Some(GroupPhase::Stopping(group)) => {
            let overrides = group.manual_overrides;
            Some(ChainStart {
                order: group
                    .restart_order
                    .into_iter()
                    .filter(|id| !overrides.contains_key(id))
                    .collect(),
                overrides,
                scope: group.scope,
            })
        }
        other => {
            sup.pending_group = other;
            None
        }
    }
}

/// Delivers one `on_child_stopped`; a failing callback stops the supervisor.
async fn deliver_child_event<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    event: &ChildEvent,
) -> ControlFlow<StopReason> {
    let clock = ctx.turn_clock();
    clock.begin();
    let delivered = catch_callback(actor.on_child_stopped(event, ctx)).await;
    clock.end();
    match delivered {
        Ok(Ok(())) => ControlFlow::Continue(()),
        Ok(Err(err)) => {
            ctx.record_failure(err.clone());
            ControlFlow::Break(StopReason::Failure(err))
        }
        Err(payload) => {
            let err = ActorError::Panic(payload_into_string(payload));
            ctx.record_failure(err.clone());
            ControlFlow::Break(StopReason::Failure(err))
        }
    }
}

/// Advances the restart chain and starts its front member when that member
/// is down.
fn advance_and_initiate<A: Actor>(
    ctx: &mut ActorContext<A>,
    work: &mut VecDeque<ChildStoppedInternal>,
) {
    let step = ctx
        .supervision_mut()
        .map(|sup| advance_restart_chain(sup, work));
    if let Some(ChainStep::Initiate(front)) = step {
        initiate_restart(ctx, &front, Origin::Chain);
    }
}

/// Handles child-death events: manual-completion delivery, stale-report
/// filtering, group-restart bookkeeping, strategy evaluation, restart
/// initiation, and the `on_child_stopped` callback.
///
/// Every report is matched exactly: by the child and the incarnation it is
/// of. A report of the ledger entry's occupant, or of an end the entry owes
/// its restart decision, is handled; a report of any other incarnation
/// changes nothing but the pending snapshot of a completed manual stop.
///
/// Runs as a worklist so that a completing group restart can drain the
/// failure events that were queued while the group was pending.
async fn handle_child_stopped<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    event: ChildStoppedInternal,
) -> ControlFlow<StopReason> {
    let mut work: VecDeque<ChildStoppedInternal> = VecDeque::new();
    work.push_back(event);
    let flow = handle_worklist(actor, ctx, &mut work).await;
    if flow.is_break() {
        // The loop ends with reports still in the worklist: they are
        // collected for their own deliveries.
        if let Some(sup) = ctx.supervision_mut() {
            sup.owed.extend(work);
        }
    }
    flow
}

/// Runs the worklist of [`handle_child_stopped`] until it is empty.
async fn handle_worklist<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    work: &mut VecDeque<ChildStoppedInternal>,
) -> ControlFlow<StopReason> {
    while let Some(ev) = work.pop_front() {
        // Manual-completion delivery: `stop_child`/`terminate_child` already
        // classified this exact incarnation's disposition the moment its real
        // fate was observed (see `ActorContext::manual_stop_child`) and
        // stored the resulting snapshot here, independent of whatever the
        // registry entry looks like by now - a same-handler `delete_child`
        // and respawn of the same name may already have replaced or removed
        // it. This is the ONLY place `on_child_stopped` fires for a manual
        // stop, and it fires from the snapshot, exactly once.
        //
        // A snapshot belongs to an incarnation the ledger entry has moved
        // past: the report of the entry's occupant, or of an end it owes its
        // decision, is never delivered from one.
        let snapshot = ctx.supervision_mut().and_then(|sup| {
            let class = sup
                .registry
                .get(&ev.child_id)
                .map(|child| rules::report_class(child.lifecycle, ev.incarnation));
            if matches!(
                class,
                Some(ReportClass::Occupant | ReportClass::OwedEvaluation)
            ) {
                return None;
            }
            sup.pending_manual_events
                .remove(&(ev.child_id.clone(), ev.incarnation))
        });
        if let Some(child_event) = snapshot {
            // This death may ALSO be the individual death an in-flight
            // group's `Stopping` phase is awaiting (a `terminate_child`
            // override recorded while that phase was in flight - see
            // `ActorContext::manual_stop_child` - already settled the
            // ledger and this snapshot synchronously; the run loop still
            // owns `awaiting` removal and starting the chain once it
            // empties). Neither duty short-circuits the other: the group
            // bookkeeping below always runs first, and the snapshot always
            // fires exactly once, from here, regardless of group
            // membership. Delivering a snapshot never moves the ledger
            // beyond clearing the marker it left: the entry may already
            // hold a newer incarnation.
            let mut begin_chain = None;
            if let Some(sup) = ctx.supervision_mut() {
                if let Some(child) = sup.registry.get_mut(&ev.child_id) {
                    if child.lifecycle
                        == (ChildLifecycle::Down {
                            incarnation: ev.incarnation,
                            pending: DownPending::Snapshot,
                        })
                    {
                        let _ = child.transition(ChildLifecycle::Down {
                            incarnation: ev.incarnation,
                            pending: DownPending::None,
                        });
                    }
                }
                if settle_barrier(sup, &ev.child_id, ev.incarnation) {
                    begin_chain = advance_group_stop(sup);
                }
            }
            if let Some(start) = begin_chain {
                start_restart_chain(ctx, start, work);
            } else {
                // A front member whose manual stop just completed does not
                // hold the chain any more.
                let is_front =
                    ctx.supervision_ref()
                        .is_some_and(|sup| match sup.pending_group.as_ref() {
                            Some(GroupPhase::Restarting { queue, .. }) => {
                                queue.front() == Some(&ev.child_id)
                            }
                            _ => false,
                        });
                if is_front {
                    advance_and_initiate(ctx, work);
                }
            }
            deliver_child_event(actor, ctx, &child_event).await?;
            continue;
        }

        // Cancel-safety fallback: the ledger is still `Stopping{kind}` for
        // this exact incarnation, meaning a caller committed a manual stop
        // and then dropped its own future before ever observing the fate (no
        // snapshot was ever recorded above). The commit already happened
        // (synchronously, before any await) and this death IS that commit's
        // own result, so the run loop finishes the classification itself
        // instead of falling through to the strategy-evaluated crash path.
        let uncommitted_manual = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&ev.child_id))
            .and_then(|child| match child.lifecycle {
                ChildLifecycle::Stopping { incarnation, kind } if incarnation == ev.incarnation => {
                    Some(kind)
                }
                _ => None,
            });
        if let Some(kind) = uncommitted_manual {
            let removed = ctx.classify_manual_completion(
                &ev.child_id,
                ev.incarnation,
                kind,
                ev.reason.clone(),
            );
            work.push_front(ev);
            // The report goes on through the worklist; on a panic of the
            // factory's drop it stays there, first, for the final deliveries.
            if let Some(err) = drop_removed(ctx, removed) {
                return ControlFlow::Break(StopReason::Failure(err));
            }
            continue;
        }

        // Stale-report filter: a report of an incarnation the entry has
        // moved past is not a death of the child the registry tracks.
        let stale = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&ev.child_id))
            .is_some_and(|child| {
                rules::report_class(child.lifecycle, ev.incarnation) == ReportClass::Stale
            });
        if stale {
            continue;
        }

        let mut steps = GroupSteps::default();
        let routed = match ctx.supervision_mut() {
            Some(sup) => route_report(sup, &ev, &mut steps),
            None => Routed::Evaluate,
        };

        match routed {
            Routed::Queued => continue,
            Routed::Deliver(mut action) => {
                // A caller's `terminate_child` on this member during the
                // group's Stopping phase wins over the group's default
                // disposition: it is reported and left down instead of being
                // folded into the restart chain (the member left the restart
                // order when the call was made).
                if let Some(start) = steps.begin_chain.take() {
                    start_restart_chain(ctx, start, work);
                } else if steps.advance_chain {
                    advance_and_initiate(ctx, work);
                }
                let child_name = ctx
                    .supervision_ref()
                    .and_then(|sup| sup.registry.get(&ev.child_id))
                    .and_then(|child| child.name.clone());
                let mut removed = None;
                if steps.manual_override {
                    removed = prune_spec_if_needed(ctx, &ev.child_id);
                    action = SupervisionAction::Removed;
                }
                if let Some(err) = drop_removed(ctx, removed) {
                    owe_report(ctx, ev, child_name);
                    return ControlFlow::Break(StopReason::Failure(err));
                }
                let child_event = ChildEvent {
                    child_id: ev.child_id.clone(),
                    child_name,
                    reason: ev.reason.clone(),
                    action,
                };
                deliver_child_event(actor, ctx, &child_event).await?;
                continue;
            }
            Routed::ChainRetry => {
                // The in-flight member's restart failed (factory panic,
                // spawn error, or the fresh incarnation died before
                // adoption): the whole group is laid out again, charging the
                // budget once (OTP try_again_restart parity).
                let budget_ok = ctx
                    .supervision_mut()
                    .map(|sup| sup.budget.check_and_record())
                    .unwrap_or(false);
                if budget_ok {
                    retry_group(actor, ctx, work, &ev).await?;
                    continue;
                }
                let child_name = ctx
                    .supervision_ref()
                    .and_then(|sup| sup.registry.get(&ev.child_id))
                    .and_then(|child| child.name.clone());
                let child_event = ChildEvent {
                    child_id: ev.child_id.clone(),
                    child_name,
                    reason: ev.reason.clone(),
                    action: SupervisionAction::BudgetExhausted,
                };
                deliver_child_event(actor, ctx, &child_event).await?;
                ctx.record_failure(SupervisionError::BudgetExhausted.into());
                return ControlFlow::Break(StopReason::ParentRequest);
            }
            Routed::Evaluate => {}
        }

        // Normal path: the ledger holds the end; evaluate the strategy, act.
        let child_name = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&ev.child_id))
            .and_then(|child| child.name.clone());

        let outcome = ctx
            .supervision_mut()
            .map(|sup| evaluate_strategy(sup, &ev.child_id, &ev.reason));

        let mut removed = None;
        let action = match outcome {
            None => SupervisionAction::NotSupervised,
            Some(StrategyOutcome::RestartOne(id)) => {
                initiate_restart(ctx, &id, Origin::Strategy);
                SupervisionAction::RestartInitiated
            }
            Some(StrategyOutcome::RestartGroup { commit }) => {
                // Owed evaluations of the group's members are absorbed into
                // the group first, then the group's stops are carried out.
                if let ControlFlow::Break(reason) = absorb_owed(actor, ctx, work, &commit).await {
                    // The trigger's own report was taken, charged and
                    // recorded, and its delivery has not run: it stays owed
                    // ahead of the absorbed reports the failing delivery
                    // left, for the final collection.
                    if let Some(sup) = ctx.supervision_mut() {
                        sup.owed.push_front(ev);
                    }
                    return ControlFlow::Break(reason);
                }
                let restart_order: Vec<ActorId> = commit
                    .iter()
                    .filter(|(_, _, action)| action.restarts)
                    .map(|(id, _, _)| id.clone())
                    .collect();
                // The scope the group keeps through its retries: under
                // `RestForOne`, the keys before the first failed member stay
                // outside it.
                let scope = GroupScope {
                    restarted: HashSet::new(),
                    outside: ctx
                        .supervision_ref()
                        .map(|sup| sup.first_plan_outside(&ev.child_id))
                        .unwrap_or_default(),
                };
                let (awaiting, stop_queue) = match ctx.supervision_ref() {
                    Some(sup) => plan_group_stop(sup, &commit),
                    None => (HashMap::new(), VecDeque::new()),
                };
                if awaiting.is_empty() {
                    // No live members to stop: go straight to the sequential
                    // restart chain.
                    start_restart_chain(
                        ctx,
                        ChainStart {
                            order: restart_order,
                            overrides: HashMap::new(),
                            scope,
                        },
                        work,
                    );
                } else if let Some(sup) = ctx.supervision_mut() {
                    sup.pending_group = Some(GroupPhase::Stopping(GroupRestart {
                        awaiting,
                        stop_queue,
                        head_reached: false,
                        restart_order,
                        manual_overrides: HashMap::new(),
                        scope,
                    }));
                    advance_stop_queue(sup);
                }
                SupervisionAction::RestartInitiated
            }
            Some(StrategyOutcome::Remove) => {
                removed = prune_spec_if_needed(ctx, &ev.child_id);
                SupervisionAction::Removed
            }
            Some(StrategyOutcome::BudgetExhausted) => SupervisionAction::BudgetExhausted,
        };
        if let Some(err) = drop_removed(ctx, removed) {
            owe_report(ctx, ev, child_name);
            return ControlFlow::Break(StopReason::Failure(err));
        }

        let child_event = ChildEvent {
            child_id: ev.child_id.clone(),
            child_name,
            reason: ev.reason.clone(),
            action,
        };
        deliver_child_event(actor, ctx, &child_event).await?;

        if matches!(action, SupervisionAction::BudgetExhausted) {
            ctx.record_failure(SupervisionError::BudgetExhausted.into());
            // OTP parity: an intensity-exceeded supervisor exits with
            // `shutdown` (our ParentRequest), so its own supervisor's
            // Transient policy does not restart it. Direct break: not
            // vetoable by pre_stop.
            return ControlFlow::Break(StopReason::ParentRequest);
        }
    }

    ControlFlow::Continue(())
}

/// The retry of a group restart after the chain's own start of a member
/// failed, once the budget took its one charge.
///
/// The group is planned again from the registry as it is now
/// ([`SupervisionState::plan_retry`]): `OneForAll` covers every child, one
/// spawned since included, and `RestForOne` the failed member and every child
/// after it. The owed evaluations of the members are absorbed, every member
/// with an occupant is committed (a member whose restart attempt is in flight
/// is ordered and cancelled, one already stopping is joined), and the group
/// returns to the stop phase with the overrides it kept, so a member a
/// `terminate_child` released stays down. When no member has an occupant, the
/// chain starts in the same step. The failed attempt's own end is delivered
/// once with `RestartInitiated`, then the absorbed reports; the ledger and
/// the group hold the whole plan before any callback runs.
async fn retry_group<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    work: &mut VecDeque<ChildStoppedInternal>,
    failed: &ChildStoppedInternal,
) -> ControlFlow<StopReason> {
    let mut deliveries: VecDeque<(ChildEvent, u64)> = VecDeque::new();
    let mut removed: Vec<RestartFn> = Vec::new();
    let mut awaiting: HashMap<ActorId, u64> = HashMap::new();
    let mut stop_queue: VecDeque<ActorId> = VecDeque::new();
    let mut restart_order: Vec<ActorId> = Vec::new();
    let mut overrides: HashMap<ActorId, u64> = HashMap::new();
    let mut scope = GroupScope::default();
    if let Some(sup) = ctx.supervision_mut() {
        deliveries.push_back((
            ChildEvent {
                child_id: failed.child_id.clone(),
                child_name: sup
                    .registry
                    .get(&failed.child_id)
                    .and_then(|child| child.name.clone()),
                reason: failed.reason.clone(),
                action: SupervisionAction::RestartInitiated,
            },
            failed.incarnation,
        ));
        // The members the retry covers are absorbed first, and the group is
        // planned from the ledger they leave. A down member's stop and
        // restart do not depend on what its end still owes, so the two plans
        // cover the same members.
        let covered = sup.plan_retry(&failed.child_id);
        let (absorbed, factories) = absorb_records(sup, work, &covered.commit);
        deliveries.extend(absorbed);
        removed = factories;
        let plan = sup.plan_retry(&failed.child_id);
        (overrides, scope) = match sup.pending_group.as_ref() {
            Some(GroupPhase::Restarting {
                overrides, scope, ..
            }) => (overrides.clone(), scope.clone()),
            _ => (HashMap::new(), GroupScope::default()),
        };
        (awaiting, stop_queue) = plan_group_stop(sup, &plan.commit);
        restart_order = plan.restart_order;
    }
    if awaiting.is_empty() {
        start_restart_chain(
            ctx,
            ChainStart {
                order: restart_order,
                overrides,
                scope,
            },
            work,
        );
    } else if let Some(sup) = ctx.supervision_mut() {
        sup.pending_group = Some(GroupPhase::Stopping(GroupRestart {
            awaiting,
            stop_queue,
            head_reached: false,
            restart_order,
            manual_overrides: overrides,
            scope,
        }));
        advance_stop_queue(sup);
    }
    // The ledger and the group hold the whole plan: the factories the
    // absorption removed are dropped now, before any callback. On a panic,
    // the failed attempt's end and the absorbed reports stay owed, in order.
    if let Some(err) = drop_removed(ctx, removed) {
        owe_deliveries(ctx, deliveries);
        return ControlFlow::Break(StopReason::Failure(err));
    }
    deliver_absorbed(actor, ctx, deliveries).await
}

/// Starts the sequential restart chain for a completed group: sets the
/// Restarting phase and hands the front of the queue to
/// [`advance_restart_chain`], which skips any member already fresher-`Running`
/// before this ever initiates a first attempt (sequential registration, OTP
/// left-to-right). An empty order (or one where every member turns out to
/// already be fresh) just drains the queued triggers.
fn start_restart_chain<A: Actor>(
    ctx: &mut ActorContext<A>,
    start: ChainStart,
    work: &mut VecDeque<ChildStoppedInternal>,
) {
    let queue: VecDeque<ActorId> = start.order.into();
    let step = if let Some(sup) = ctx.supervision_mut() {
        sup.pending_group = Some(GroupPhase::Restarting {
            queue,
            overrides: start.overrides,
            started: HashMap::new(),
            scope: start.scope,
        });
        advance_restart_chain(sup, work)
    } else {
        ChainStep::Done
    };
    if let ChainStep::Initiate(front) = step {
        initiate_restart(ctx, &front, Origin::Chain);
    }
}

/// Dispatches one restart attempt's outcome: an [`RestartOutcome::Adopted`]
/// value is handed to [`adopt_restart`]; a [`RestartOutcome::Failed`] one is
/// reported through the ordinary child-death path - budget-charged strategy
/// evaluation, exactly like any other failure - by synthesizing the same
/// [`ChildStoppedInternal`] event a runner completion would have produced.
async fn handle_restart_outcome<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    outcome: RestartOutcome,
) -> ControlFlow<StopReason> {
    match outcome {
        RestartOutcome::Adopted {
            child_id,
            incarnation,
            new_stop_lane,
            new_join,
            guard,
            io,
        } => {
            adopt_restart(
                actor,
                ctx,
                child_id,
                incarnation,
                new_stop_lane,
                new_join,
                guard,
                io,
            )
            .await
        }
        RestartOutcome::Failed {
            child_id,
            incarnation,
            reason,
        }
        | RestartOutcome::Stopped {
            child_id,
            incarnation,
            reason,
        } => {
            // An attempt's incarnation that ended under a stop order is
            // reported like a failed one: the end of the incarnation the
            // ledger holds as the attempt's.
            let event = ChildStoppedInternal {
                child_id,
                reason,
                incarnation,
            };
            handle_child_stopped(actor, ctx, event).await
        }
        RestartOutcome::Cancelled {
            child_id,
            incarnation,
            drop_panic,
        } => {
            // The attempt honored a stop order or the group's cancel before its
            // factory ran (or while it waited for the previous subtree), or
            // found the spec removed: no incarnation existed, and nothing is
            // delivered.
            let mut begin_chain = None;
            if let Some(sup) = ctx.supervision_mut() {
                record_cancelled(sup, &child_id, incarnation);
                if settle_barrier(sup, &child_id, incarnation) {
                    begin_chain = advance_group_stop(sup);
                }
            }
            let chained = begin_chain.is_some();
            let mut released = VecDeque::new();
            if let Some(start) = begin_chain {
                start_restart_chain(ctx, start, &mut released);
            }
            // The factory's last reference dropped in the attempt, and its
            // drop panicked: a panic of the turn, recorded as a handler's is,
            // once the outcome is recorded. The reports the chain released
            // stay owed, in order.
            if let Some(payload) = drop_panic {
                let err = ActorError::Panic(payload);
                ctx.record_failure(err.clone());
                if let Some(sup) = ctx.supervision_mut() {
                    sup.owed.extend(released);
                }
                return ControlFlow::Break(StopReason::Failure(err));
            }
            if !chained {
                return ControlFlow::Continue(());
            }
            handle_released(actor, ctx, released).await
        }
        RestartOutcome::PredecessorUnresponsive {
            child_id,
            incarnation,
        } => {
            // The attempt's wait for the previous incarnation's subtree
            // outlived its bound: no incarnation existed, so the ledger entry
            // is down with nothing owed, and nothing is charged, delivered or
            // evaluated. The supervisor leaves its loop with the failure, so
            // its own supervisor decides.
            if let Some(sup) = ctx.supervision_mut() {
                record_cancelled(sup, &child_id, incarnation);
            }
            let err: ActorError = SupervisionError::ChildUnresponsive(child_id).into();
            ctx.record_failure(err.clone());
            ControlFlow::Break(StopReason::Failure(err))
        }
    }
}

/// Records the end of an attempt that never produced an incarnation: the
/// ledger entry the attempt held reaches `Down` with nothing owed.
fn record_cancelled(sup: &mut SupervisionState, child_id: &ActorId, incarnation: u64) {
    if let Some(child) = sup.registry.get_mut(child_id) {
        if matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == incarnation)
        {
            let _ = child.transition(ChildLifecycle::Down {
                incarnation,
                pending: DownPending::None,
            });
        }
    }
}

/// Adopts a restart attempt's [`RestartOutcome::Adopted`] value: validates
/// the incarnation token, starts the fresh instance's runner on the slot the
/// attempt hands over (the guard itself was already armed inside the restart
/// task, the instant its `spawn_actor` call succeeded; a stop order written
/// while the outcome was on its way is honored by the runner at once),
/// advances a sequential group-restart
/// chain, and - when the chain completes - processes the failure events that
/// were queued while the group was in flight.
///
/// A member adopted while the group is still in its `Stopping` phase is one
/// whose attempt the group committed when it was evaluated. Its barrier entry
/// names the attempt's incarnation already, and the adoption writes no order:
/// when the stop queue reached the member the slot holds the queue's order
/// and the runner honors it at once, and otherwise the queue orders the
/// adopted incarnation when its turn comes, so it is stopped and started
/// again in its slot, never kept.
#[allow(clippy::too_many_arguments)]
async fn adopt_restart<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    child_id: ActorId,
    seq: u64,
    new_stop_lane: StopLane,
    new_join: JoinHandle<StopReason>,
    guard: ChildLinkGuard,
    io: SlotIo,
) -> ControlFlow<StopReason> {
    let accepted = ctx
        .supervision_ref()
        .and_then(|sup| sup.registry.get(&child_id))
        .map(|child| matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == seq))
        .unwrap_or(false);
    // A slot leaves `Restarting` only through its own attempt's outcome, and
    // no report about the attempt's incarnation exists before its adoption,
    // so an outcome that finds another entry is not expected.
    debug_assert!(
        accepted,
        "an adoption concerns the attempt the ledger holds"
    );
    if !accepted {
        // Superseded restart (a newer seq is pending or the spec is gone):
        // dropping the guard kills the stray instance through the ordinary
        // Kill-raise/reaper ladder instead of leaking a
        // duplicate. Its death event would carry a stale incarnation and be
        // ignored anyway.
        drop(guard);
        drop(new_join);
        return ControlFlow::Continue(());
    }

    let mut step = ChainStep::Hold;
    let mut drained: VecDeque<ChildStoppedInternal> = VecDeque::new();
    if let Some(sup) = ctx.supervision_mut() {
        // The kill grace of the child's spec: the one its link guard was
        // built with.
        let kill_grace = sup
            .registry
            .get(&child_id)
            .map_or(DEFAULT_KILL_GRACE, |child| child.spec.kill_grace);
        let task = spawn_runner(
            &mut sup.death_set,
            child_id.clone(),
            seq,
            new_join,
            new_stop_lane,
            io,
            guard,
            kill_grace,
        );
        sup.task_owner
            .insert(task.id(), (child_id.clone(), seq, TaskPlane::Death));
        sup.registry.update_restarted(&child_id, seq);
        if let Some(child) = sup.registry.get_mut(&child_id) {
            child.note_subtree();
        }

        let occupant = sup
            .registry
            .get(&child_id)
            .and_then(|child| child.lifecycle.occupant());
        let folds = matches!(
            sup.pending_group.as_ref(),
            Some(GroupPhase::Stopping(group)) if group.restart_order.contains(&child_id)
        );
        let is_front = matches!(
            sup.pending_group.as_ref(),
            Some(GroupPhase::Restarting { queue, .. }) if queue.front() == Some(&child_id)
        );
        if folds {
            if let (Some(GroupPhase::Stopping(group)), Some(occupant)) =
                (sup.pending_group.as_mut(), occupant)
            {
                // The group committed the attempt when it was evaluated, so
                // its barrier entry names this very incarnation already.
                debug_assert!(
                    group.awaiting.get(&child_id) == Some(&occupant),
                    "an attempt adopted during the stop phase was committed to the group"
                );
                group.awaiting.insert(child_id.clone(), occupant);
            }
        } else if is_front {
            step = advance_restart_chain(sup, &mut drained);
        }
    }

    if let ChainStep::Initiate(next) = step {
        initiate_restart(ctx, &next, Origin::Chain);
    }
    handle_released(actor, ctx, drained).await
}

/// Handles the reports a completing chain released as one worklist: an
/// evaluation among them finds the others still waiting in it. When a
/// delivery stops the supervisor, the reports not yet handled stay owed for
/// its final collection.
async fn handle_released<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    mut released: VecDeque<ChildStoppedInternal>,
) -> ControlFlow<StopReason> {
    let flow = handle_worklist(actor, ctx, &mut released).await;
    if flow.is_break() {
        if let Some(sup) = ctx.supervision_mut() {
            sup.owed.extend(released);
        }
    }
    flow
}

/// Commits a group restart's members to its stop phase, from the commit list
/// of the evaluation or of a retry's plan: the barrier holds every member
/// with an occupant at that occupant's incarnation, the stop queue holds the
/// same members in reverse start order, and the restart attempt of a member
/// that has not spawned its actor yet is cancelled at once. Nothing is
/// stopped here: the queue stops one member at a time, each in its turn.
fn plan_group_stop(
    sup: &SupervisionState,
    commit: &[(ActorId, u64, rules::MemberAction)],
) -> (HashMap<ActorId, u64>, VecDeque<ActorId>) {
    let awaiting = commit
        .iter()
        .filter(|(_, _, action)| action.held)
        .map(|(id, token, _)| (id.clone(), *token))
        .collect();
    let stop_queue = commit
        .iter()
        .rev()
        .filter(|(_, _, action)| action.held)
        .map(|(id, _, _)| id.clone())
        .collect();
    for (id, _, action) in commit {
        if action.cancel {
            if let Some(child) = sup.registry.get(id) {
                child.ctl.cancel_unspawned();
            }
        }
    }
    (awaiting, stop_queue)
}

/// What the stop queue reads of its head: the barrier's entry, the slot's
/// order and request state, and whether a completed manual stop classified
/// the head's end already. The ledger entry and the slot count only when they
/// hold the incarnation the barrier names: the end of an incarnation the key
/// has moved past is waited for, and the key's new occupant is no member.
pub(crate) fn queue_head(
    sup: &SupervisionState,
    group: &GroupRestart,
    id: &ActorId,
) -> rules::QueueHead {
    let awaited = group.awaiting.get(id).copied();
    let child = sup.registry.get(id);
    let held = awaited.zip(child);
    let slot = held.and_then(|(token, child)| child.slot_of(token).map(|ctl| (child, ctl)));
    rules::QueueHead {
        awaited: awaited.is_some(),
        verdict: slot.is_some_and(|(_, ctl)| ctl.is_unresponsive()),
        signalled: group.head_reached,
        handled: awaited
            .is_some_and(|token| sup.pending_manual_events.contains_key(&(id.clone(), token))),
        life: slot.map(|(child, _)| child.lifecycle),
        has_order: slot.is_some_and(|(_, ctl)| ctl.has_order()),
        ask_possible: slot.is_some_and(|(child, ctl)| {
            child.spec.shutdown == Shutdown::Infinity
                && ctl.lane_vetoable()
                && ctl.task_state() != TaskState::Ended
        }),
        reasked: slot.is_some_and(|(_, ctl)| ctl.reasked()),
        attempt_past_init: slot.is_some_and(|(child, ctl)| {
            matches!(child.lifecycle, ChildLifecycle::Restarting { .. })
                && ctl
                    .status()
                    .is_some_and(|status| status.borrow().status != ActorStatus::Initializing)
        }),
        attempt_spawned: slot.is_none_or(|(child, ctl)| {
            !matches!(child.lifecycle, ChildLifecycle::Restarting { .. }) || ctl.lane().is_some()
        }),
    }
}

/// The group restart's stop queue, one member at a time. The head is decided
/// from its state now ([`rules::stop_queue_next`]): a member whose end was
/// delivered, who carries the Unresponsive verdict or whose end a completed
/// manual stop classified is passed over; a running member is ordered to stop
/// under its own `Shutdown` policy, a member whose stop is in progress is
/// joined (never given a second ladder, and asked once more when it is a
/// vetoing `Infinity` member that nobody asked since), and a member whose
/// restart attempt is in flight has the attempt ordered to stop. Each
/// member's runner raises the first signal, escalates at the policy's
/// deadline to `Kill` and then to an abort, and records the member's end; the
/// queue reads the next member only when that end, or a verdict, is recorded.
/// Nothing here waits or arms a timer.
fn advance_stop_queue(sup: &mut SupervisionState) {
    loop {
        let Some(GroupPhase::Stopping(group)) = sup.pending_group.as_ref() else {
            return;
        };
        let Some(head) = group.stop_queue.front().cloned() else {
            return;
        };
        let step = rules::stop_queue_next(&queue_head(sup, group, &head));
        if step == rules::QueueStep::Wait {
            return;
        }
        if let Some(child) = sup.registry.get(&head) {
            match step {
                rules::QueueStep::Order => child.ctl.order_stop(child.spec.shutdown, true),
                rules::QueueStep::OrderUnpublished => {
                    child.ctl.order_stop(child.spec.shutdown, false);
                }
                rules::QueueStep::OrderAttempt { ask } => {
                    child.ctl.order_stop(child.spec.shutdown, true);
                    if ask {
                        child.ctl.ask_again();
                        child.ctl.mark_reasked();
                    }
                }
                rules::QueueStep::Join { ask } => {
                    if ask {
                        child.ctl.ask_again();
                        child.ctl.mark_reasked();
                    }
                }
                rules::QueueStep::Pop | rules::QueueStep::Wait | rules::QueueStep::Await => {}
            }
        }
        if let Some(GroupPhase::Stopping(group)) = sup.pending_group.as_mut() {
            let reached = rules::queue_head_reached_after(step, group.head_reached);
            if step == rules::QueueStep::Pop {
                group.stop_queue.pop_front();
            }
            // A signalled head's next read says to wait.
            group.head_reached = reached;
        }
    }
}

/// Marks the incarnation of `child_id` the stop phase awaits as ended.
/// Returns `false` when no stop phase is pending or it awaits another
/// incarnation.
fn settle_barrier(sup: &mut SupervisionState, child_id: &ActorId, incarnation: u64) -> bool {
    match sup.pending_group.as_mut() {
        Some(GroupPhase::Stopping(group)) => group.settle_awaited(child_id, incarnation),
        _ => false,
    }
}

/// The stop phase's step after a barrier entry was settled or a verdict was
/// given: the queue goes on to the next member, and the chain starts, with
/// the restart order the group kept, once the queue and the barrier are both
/// empty.
fn advance_group_stop(sup: &mut SupervisionState) -> Option<ChainStart> {
    advance_stop_queue(sup);
    let finished = matches!(
        sup.pending_group.as_ref(),
        Some(GroupPhase::Stopping(group))
            if group.stop_queue.is_empty() && group.awaiting.is_empty()
    );
    if finished {
        take_chain_order(sup)
    } else {
        None
    }
}

/// Absorbs the owed evaluations of a group's members into the group, before
/// the group's stops are carried out: a member's end belongs to its group, so
/// it is neither evaluated nor charged. See [`absorb_records`] for what is
/// recorded; the deliveries it names are made here, in order.
async fn absorb_owed<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    work: &mut VecDeque<ChildStoppedInternal>,
    commit: &[(ActorId, u64, rules::MemberAction)],
) -> ControlFlow<StopReason> {
    let (deliveries, removed) = match ctx.supervision_mut() {
        Some(sup) => absorb_records(sup, work, commit),
        None => (VecDeque::new(), Vec::new()),
    };
    // The factories the absorption removed are dropped once it is recorded,
    // before any delivery. On a panic, the absorbed reports stay owed.
    if let Some(err) = drop_removed(ctx, removed) {
        owe_deliveries(ctx, deliveries);
        return ControlFlow::Break(StopReason::Failure(err));
    }
    deliver_absorbed(actor, ctx, deliveries).await
}

/// Records the absorption of every owed evaluation of a group's members, and
/// returns the deliveries it owes, each with the incarnation it concerns. A
/// report waiting in the deferred triggers or in the worklist is removed, the
/// member is recorded down, and the report is delivered once as the group's
/// restart of the member. A report that has not reached the loop yet (a
/// bounce a `stop_child` met a crash on completed inline) leaves a snapshot of
/// the end that the report delivers once when it drains. A completed manual
/// stop's snapshot that says the member stays down is rewritten, for a member
/// the group restarts, to say the group restarts it. A `Temporary` member is
/// not restarted whatever its report says: its spec is removed and its end is
/// reported removed ([`rules::group_member_end`]). The factories of the
/// removed specs are returned, for the caller to drop after its last write.
fn absorb_records(
    sup: &mut SupervisionState,
    work: &mut VecDeque<ChildStoppedInternal>,
    commit: &[(ActorId, u64, rules::MemberAction)],
) -> (VecDeque<(ChildEvent, u64)>, Vec<RestartFn>) {
    let mut deliveries: VecDeque<(ChildEvent, u64)> = VecDeque::new();
    let mut removed: Vec<RestartFn> = Vec::new();
    for (id, _, action) in commit {
        let Some(child) = sup.registry.get(id) else {
            continue;
        };
        let life = child.lifecycle;
        let name = child.name.clone();
        let prune =
            rules::group_member_end(child.spec.restart_type) == rules::MemberEnd::PruneAndRemove;
        let ChildLifecycle::Down { incarnation: n, .. } = life else {
            continue;
        };
        let queued = sup
            .queued_triggers
            .iter()
            .position(|ev| &ev.child_id == id && ev.incarnation == n);
        let worklist = work
            .iter()
            .position(|ev| &ev.child_id == id && ev.incarnation == n);
        let place = if queued.is_some() {
            rules::ReportPlace::Queued
        } else if worklist.is_some() {
            rules::ReportPlace::Worklist
        } else {
            rules::ReportPlace::DeathPlane
        };
        match rules::absorbable(place, true, Some(life)) {
            rules::Absorption::Keep => {
                // A completed manual stop that kept the member down rejoins
                // the restart the group makes.
                if action.restarts {
                    if let Some(snapshot) = sup.pending_manual_events.get_mut(&(id.clone(), n)) {
                        if snapshot.action == SupervisionAction::Removed {
                            snapshot.action = SupervisionAction::RestartInitiated;
                        }
                    }
                }
            }
            rules::Absorption::Deliver => {
                let report = match (queued, worklist) {
                    (Some(i), _) => sup.queued_triggers.remove(i),
                    (None, Some(i)) => work.remove(i),
                    (None, None) => None,
                };
                if let Some(report) = report {
                    if let Some(child) = sup.registry.get_mut(id) {
                        let _ = child.transition(ChildLifecycle::Down {
                            incarnation: n,
                            pending: DownPending::None,
                        });
                    }
                    if prune {
                        removed.extend(sup.remove_spec(id));
                    }
                    #[cfg(feature = "tracing")]
                    caught_log!(debug,
                        child = %id,
                        incarnation = n,
                        waited_in = report_place_words(place),
                        "an owed report of a group member was absorbed into the group restart"
                    );
                    deliveries.push_back((
                        ChildEvent {
                            child_id: id.clone(),
                            child_name: name,
                            reason: report.reason,
                            action: if prune {
                                SupervisionAction::Removed
                            } else {
                                SupervisionAction::RestartInitiated
                            },
                        },
                        n,
                    ));
                }
            }
            rules::Absorption::Snapshot => {
                let reason = sup
                    .registry
                    .get(id)
                    .map_or(StopReason::Kill, |child| child.ctl.end_reason());
                if let Some(child) = sup.registry.get_mut(id) {
                    let _ = child.transition(ChildLifecycle::Down {
                        incarnation: n,
                        pending: DownPending::Snapshot,
                    });
                }
                if prune {
                    removed.extend(sup.remove_spec(id));
                }
                #[cfg(feature = "tracing")]
                caught_log!(debug,
                    child = %id,
                    incarnation = n,
                    waited_in = report_place_words(place),
                    "the end of a group member that owes a report was recorded, to be delivered when the report arrives"
                );
                sup.pending_manual_events.insert(
                    (id.clone(), n),
                    ChildEvent {
                        child_id: id.clone(),
                        child_name: name,
                        reason,
                        action: if prune {
                            SupervisionAction::Removed
                        } else {
                            SupervisionAction::RestartInitiated
                        },
                    },
                );
            }
        }
    }
    (deliveries, removed)
}

/// Where an absorbed report waited, in words for a log line.
#[cfg(feature = "tracing")]
fn report_place_words(place: rules::ReportPlace) -> &'static str {
    match place {
        rules::ReportPlace::Queued => "deferred behind the group",
        rules::ReportPlace::Worklist => "worklist",
        rules::ReportPlace::DeathPlane => "not yet drained",
    }
}

/// Makes the deliveries [`absorb_records`] owes, in order. When a delivery
/// stops the supervisor, the ones that did not run stay owed: its final
/// collection delivers them.
async fn deliver_absorbed<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    mut deliveries: VecDeque<(ChildEvent, u64)>,
) -> ControlFlow<StopReason> {
    while let Some((event, _)) = deliveries.pop_front() {
        if let ControlFlow::Break(reason) = deliver_child_event(actor, ctx, &event).await {
            owe_deliveries(ctx, deliveries);
            return ControlFlow::Break(reason);
        }
    }
    ControlFlow::Continue(())
}

/// Prunes a child's spec after a non-restart outcome when OTP says so:
/// Temporary specs are deleted as soon as the process terminates, and
/// SimpleOneForOne removes dynamic specs likewise. A transient child stopped
/// cleanly keeps its spec (`Down`, the pid=undefined analog) for
/// restart_child. Returns the removed spec's factory, for the caller to drop
/// after its last write.
#[must_use = "the factory is dropped by the caller, after its last write"]
pub(crate) fn prune_spec_if_needed<A: Actor>(
    ctx: &mut ActorContext<A>,
    child_id: &ActorId,
) -> Option<RestartFn> {
    let sup = ctx.supervision_mut()?;
    let prune = matches!(sup.config.strategy, RestartStrategy::SimpleOneForOne)
        || sup
            .registry
            .get(child_id)
            .map(|c| matches!(c.spec.restart_type, RestartType::Temporary))
            .unwrap_or(false);
    if prune {
        sup.remove_spec(child_id)
    } else {
        None
    }
}

/// Drops the factories an operation of the run loop removed, once it has
/// made its last write. A factory is user code: each drop is caught, and a
/// panic is a panic of the turn, recorded as a handler's is and returned for
/// the turn to stop the supervisor with.
fn drop_removed<A: Actor>(
    ctx: &mut ActorContext<A>,
    removed: impl IntoIterator<Item = RestartFn>,
) -> Option<ActorError> {
    let owner = ctx.actor_id().clone();
    let mut first = None;
    for factory in removed {
        keep_first_of(&mut first, end_value(factory), &owner);
    }
    let err = ActorError::Panic(first?);
    ctx.record_failure(err.clone());
    Some(err)
}

/// Leaves a report the turn took and did not deliver for the final
/// deliveries, with the child's name.
fn owe_report<A: Actor>(
    ctx: &mut ActorContext<A>,
    report: ChildStoppedInternal,
    name: Option<String>,
) {
    if let Some(sup) = ctx.supervision_mut() {
        sup.owe(report, name);
    }
}

/// Leaves undelivered deliveries for the final deliveries, in order, each
/// with the child's name.
fn owe_deliveries<A: Actor>(ctx: &mut ActorContext<A>, deliveries: VecDeque<(ChildEvent, u64)>) {
    if let Some(sup) = ctx.supervision_mut() {
        for (event, incarnation) in deliveries {
            sup.owe(
                ChildStoppedInternal {
                    child_id: event.child_id,
                    reason: event.reason,
                    incarnation,
                },
                event.child_name,
            );
        }
    }
}

/// Initiates a non-blocking restart for a child using its stored restart
/// closure (which captured the child's id, name, and config by value).
fn initiate_restart<A: Actor>(ctx: &mut ActorContext<A>, child_id: &ActorId, origin: Origin) {
    let seq = ctx.resolved_system().next_incarnation();
    if let Some(sup) = ctx.supervision_mut() {
        sup.initiate(child_id, seq, origin);
    }
}

/// A pending group restart that can make progress only through members that
/// carry the Unresponsive verdict ends the supervisor: it records and stops
/// with `ChildUnresponsive` for the first such member in reverse start order,
/// so its own parent decides.
///
/// In the stop phase the members are those the barrier still waits for; in
/// the chain phase it is the front member's own restart attempt.
fn check_verdicts<A: Actor>(ctx: &mut ActorContext<A>) -> Option<StopReason> {
    let sup = ctx.supervision_ref()?;
    let entries: Vec<(ActorId, bool)> = match sup.pending_group.as_ref()? {
        GroupPhase::Stopping(group) => {
            let mut entries: Vec<(ActorId, bool)> = sup
                .registry
                .iter()
                .filter_map(|child| {
                    let awaited = *group.awaiting.get(&child.id)?;
                    let verdict = child
                        .slot_of(awaited)
                        .is_some_and(|ctl| ctl.is_unresponsive());
                    Some((child.id.clone(), verdict))
                })
                .collect();
            entries.reverse();
            entries
        }
        GroupPhase::Restarting { queue, started, .. } => {
            let front = queue.front()?;
            let child = sup.registry.get(front)?;
            match child.lifecycle {
                ChildLifecycle::Restarting { next, .. } if started.get(front) == Some(&next) => {
                    vec![(front.clone(), child.ctl.is_unresponsive())]
                }
                _ => Vec::new(),
            }
        }
    };
    let id = rules::barrier_blocked_only_by_verdicts(&entries)?;
    let err: ActorError = SupervisionError::ChildUnresponsive(id).into();
    ctx.record_failure(err.clone());
    Some(StopReason::Failure(err))
}

/// What the teardown reads of a child when its turn comes.
fn walk_child(child: &ChildState) -> rules::WalkChild {
    let ctl = &child.ctl;
    let ask_possible = child.spec.shutdown == Shutdown::Infinity
        && ctl.lane_vetoable()
        && ctl.task_state() != TaskState::Ended;
    rules::WalkChild {
        life: Some(child.lifecycle),
        has_order: ctl.has_order(),
        ask_possible,
        reasked: ctl.reasked(),
        attempt_past_init: ctl
            .status()
            .is_some_and(|status| status.borrow().status != ActorStatus::Initializing),
    }
}

/// Logs the end of a child its supervisor's teardown stopped, when the end
/// is not an orderly one. Changes no state and runs no callback.
#[allow(unused_variables)]
fn warn_unexpected_end(
    supervisor: &ActorId,
    child: &ActorId,
    incarnation: u64,
    reason: &StopReason,
    restart_type: RestartType,
    shutdown: Shutdown,
) {
    if !rules::teardown_end_is_orderly(reason, restart_type, shutdown) {
        #[cfg(feature = "tracing")]
        caught_log!(warn,
            supervisor = %supervisor,
            child = %child,
            incarnation,
            reason = %reason,
            "child ended unexpectedly while its supervisor stopped it"
        );
    }
}

/// Records one restart-plane outcome during the supervisor's teardown or its
/// final collection: nothing is delivered, charged or restarted. An adopted
/// attempt becomes the child's running incarnation (its runner honors the
/// order the cell holds, if any); every other outcome puts the ledger entry
/// the attempt held down. A cancelled attempt's factory drop panic is kept in
/// `drop_panic`, after any earlier one, as a drop before the reason is final.
/// Returns the child and incarnation the outcome concerned, whether the
/// attempt was adopted, and the end it reported, if any.
fn record_restart_outcome(
    sup: &mut SupervisionState,
    supervisor: &ActorId,
    outcome: RestartOutcome,
    warn: bool,
    drop_panic: &mut Option<String>,
) -> (ActorId, u64, Option<AdoptedRunner>, Option<StopReason>) {
    match outcome {
        RestartOutcome::Adopted {
            child_id,
            incarnation,
            new_stop_lane,
            new_join,
            guard,
            io,
        } => {
            let accepted = sup.registry.get(&child_id).is_some_and(|child| {
                matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == incarnation)
            });
            if !accepted {
                // Superseded: dropping the guard kills the stray
                // incarnation through the ordinary ladder.
                drop(guard);
                drop(new_join);
                return (child_id, incarnation, None, None);
            }
            let kill_grace = sup
                .registry
                .get(&child_id)
                .map_or(DEFAULT_KILL_GRACE, |child| child.spec.kill_grace);
            let task = spawn_runner(
                &mut sup.death_set,
                child_id.clone(),
                incarnation,
                new_join,
                new_stop_lane,
                io,
                guard,
                kill_grace,
            );
            sup.task_owner
                .insert(task.id(), (child_id.clone(), incarnation, TaskPlane::Death));
            sup.registry.update_restarted(&child_id, incarnation);
            if let Some(child) = sup.registry.get_mut(&child_id) {
                child.note_subtree();
            }
            let ended = sup
                .registry
                .get(&child_id)
                .is_some_and(|child| child.ctl.status_ended());
            (
                child_id,
                incarnation,
                Some(AdoptedRunner {
                    task: task.id(),
                    ended,
                }),
                None,
            )
        }
        RestartOutcome::Failed {
            child_id,
            incarnation,
            reason,
        }
        | RestartOutcome::Stopped {
            child_id,
            incarnation,
            reason,
        } => {
            if let (true, Some(child)) = (warn, sup.registry.get(&child_id)) {
                warn_unexpected_end(
                    supervisor,
                    &child_id,
                    incarnation,
                    &reason,
                    child.spec.restart_type,
                    child.spec.shutdown,
                );
            }
            (child_id, incarnation, None, Some(reason))
        }
        RestartOutcome::Cancelled {
            child_id,
            incarnation,
            drop_panic: panic,
        } => {
            record_cancelled(sup, &child_id, incarnation);
            keep_first_of(drop_panic, panic, supervisor);
            (child_id, incarnation, None, None)
        }
        RestartOutcome::PredecessorUnresponsive {
            child_id,
            incarnation,
        } => {
            record_cancelled(sup, &child_id, incarnation);
            (child_id, incarnation, None, None)
        }
    }
}

#[cfg(test)]
#[path = "end_order_tests.rs"]
mod end_order_tests;

#[cfg(test)]
#[path = "own_unwind_tests.rs"]
mod own_unwind_tests;

#[cfg(test)]
mod keep_first_tests {
    use super::*;

    /// The first panic is kept; each later one is discarded and, with the
    /// `tracing` feature, logged, naming the actor.
    #[test]
    fn a_discarded_panic_is_logged_naming_the_actor() {
        #[cfg(feature = "tracing")]
        let logged = crate::actor::supervision::lost_runner::capture::install();
        let token = uuid::Uuid::new_v4();
        let owner = ActorId::from(format!("keep-first-{token}"));
        let mut first = None;
        keep_first_of(&mut first, None, &owner);
        keep_first_of(&mut first, Some(format!("kept {token}")), &owner);
        keep_first_of(&mut first, Some(format!("second {token}")), &owner);
        keep_first_of(&mut first, Some(format!("third {token}")), &owner);
        keep_first_of(&mut first, None, &owner);
        assert_eq!(first, Some(format!("kept {token}")));
        #[cfg(feature = "tracing")]
        {
            let lines: Vec<String> = logged
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains(&token.to_string()))
                .cloned()
                .collect();
            assert_eq!(lines.len(), 2, "{lines:?}");
            for which in ["second", "third"] {
                assert!(
                    lines
                        .iter()
                        .any(|line| line.contains(which)
                            && line.contains(&format!("actor_id={owner} "))),
                    "{which}: {lines:?}"
                );
            }
            assert!(!lines
                .iter()
                .any(|line| line.contains(&format!("kept {token}"))));
        }
    }
}

#[cfg(test)]
mod teardown_adoption_tests {
    use super::*;
    use crate::actor::subtree::SubtreeHold;
    use crate::actor::supervision::{
        slot_pair, ChildSpec, ChildState, Predecessor, SupervisionConfig, DEFAULT_KILL_GRACE,
    };

    struct Idle;

    impl Actor for Idle {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    /// An attempt adopted while its supervisor tears down makes its
    /// incarnation the occupant, and the subtree of that incarnation the one a
    /// later start for the child waits for.
    #[tokio::test]
    async fn an_attempt_adopted_in_the_teardown_records_its_subtree() {
        let system = ActorSystem::create(format!("adopted-{}", uuid::Uuid::new_v4())).unwrap();
        let id = ActorId::from("child");
        let (ctl, io) = slot_pair(Default::default());
        let earlier = SubtreeHold::new(None);
        let mut sup = SupervisionState::new(SupervisionConfig::default());
        sup.registry.register(ChildState {
            id: id.clone(),
            name: None,
            spec: ChildSpec {
                restart_type: RestartType::Permanent,
                shutdown: Shutdown::default(),
                start_timeout: None,
                kill_grace: DEFAULT_KILL_GRACE,
                is_supervisor: false,
            },
            ctl,
            lifecycle: ChildLifecycle::Restarting {
                incarnation: 1,
                next: 2,
            },
            origin: Origin::Strategy,
            predecessor: Some(Predecessor::untracked(earlier.watch())),
        });
        let (handle, join, subtree) = spawn_actor(
            id.clone(),
            Idle,
            ActorConfig::default(),
            None,
            Some(system.clone()),
            None,
            false,
            DEFAULT_KILL_GRACE,
            None,
        )
        .expect("the incarnation spawns");
        io.publish(
            handle.stop_lane(),
            handle.status_rx(),
            crate::actor::supervision::OwnStop::worker(DEFAULT_KILL_GRACE),
            Some(subtree),
        );
        let guard = ChildLinkGuard::new(
            handle.stop_lane(),
            join.abort_handle(),
            system.reaper_handle(&Handle::current()),
            crate::actor::supervision::rules::InheritedStop::Kill {
                grace: DEFAULT_KILL_GRACE,
            },
            system.clone(),
            Handle::current(),
        );
        let outcome = RestartOutcome::Adopted {
            child_id: id.clone(),
            incarnation: 2,
            new_stop_lane: handle.stop_lane(),
            new_join: join,
            guard,
            io,
        };
        let supervisor = ActorId::from("supervisor");
        let (_, _, adopted, _) =
            record_restart_outcome(&mut sup, &supervisor, outcome, false, &mut None);
        assert!(adopted.is_some(), "the attempt is adopted");
        // The earlier incarnation's subtree has ended; the adopted one runs.
        drop(earlier);
        let recorded = sup
            .registry
            .get(&id)
            .and_then(|child| child.predecessor.clone())
            .expect("a predecessor is recorded");
        assert!(
            !recorded.is_gone(),
            "the recorded subtree is the adopted incarnation's, which still runs"
        );
    }
}

/// The runner an adoption spawned: its task, and whether the incarnation's
/// task had already ended when it was adopted.
struct AdoptedRunner {
    task: tokio::task::Id,
    ended: bool,
}

/// Records, with no delivery, that an incarnation's end was collected: the
/// ledger entry that held it reaches `Down` with nothing owed, a pending
/// snapshot of the end is dropped (the collected report keeps its own
/// reason), and a barrier entry for it is released.
fn record_collected_end(sup: &mut SupervisionState, child_id: &ActorId, incarnation: u64) {
    if let Some(child) = sup.registry.get_mut(child_id) {
        let held = match child.lifecycle {
            ChildLifecycle::Running(n) | ChildLifecycle::Stopping { incarnation: n, .. } => n,
            ChildLifecycle::Restarting { next, .. } => next,
            ChildLifecycle::Down { incarnation: n, .. } => n,
        };
        if held == incarnation {
            let _ = child.transition(ChildLifecycle::Down {
                incarnation,
                pending: DownPending::None,
            });
        }
    }
    sup.pending_manual_events
        .remove(&(child_id.clone(), incarnation));
    if let Some(GroupPhase::Stopping(group)) = sup.pending_group.as_mut() {
        if group.awaiting.get(child_id) == Some(&incarnation) {
            group.awaiting.remove(child_id);
        }
    }
}

/// Drains one restart-plane completion during the teardown: records it,
/// keeping a factory drop panic it carries in `drop_panic`, and returns the
/// child and incarnation it concerned, and whether the attempt became a
/// running incarnation.
fn drain_restart_for_teardown<A: Actor>(
    ctx: &mut ActorContext<A>,
    joined: Result<(tokio::task::Id, RestartOutcome), JoinError>,
    drop_panic: &mut Option<String>,
) -> Option<(ActorId, u64, bool)> {
    let supervisor = ctx.actor_id().clone();
    match joined {
        Ok((task, outcome)) => {
            let sup = ctx.supervision_mut()?;
            sup.task_owner.remove(&task);
            let (child, incarnation, adopted, ended) =
                record_restart_outcome(sup, &supervisor, outcome, true, drop_panic);
            if ended.is_some() {
                record_collected_end(sup, &child, incarnation);
            }
            Some((child, incarnation, adopted.is_some()))
        }
        Err(err) => {
            let owner = ctx
                .supervision_ref()
                .and_then(|sup| sup.task_owner.get(&err.id()).cloned());
            let (outcome, _) = ctx.lost_restart_task(err);
            match outcome {
                Some(outcome) => {
                    let sup = ctx.supervision_mut()?;
                    let (child, incarnation, adopted, ended) =
                        record_restart_outcome(sup, &supervisor, outcome, true, drop_panic);
                    if ended.is_some() {
                        record_collected_end(sup, &child, incarnation);
                    }
                    Some((child, incarnation, adopted.is_some()))
                }
                None => owner.map(|(child, incarnation, _)| (child, incarnation, false)),
            }
        }
    }
}

/// One stop the teardown waits for: the child, the incarnation whose end
/// settles it, whether the incarnation is a restart attempt, and the watch on
/// its slot's cell.
struct StopWait {
    id: ActorId,
    token: u64,
    attempt: bool,
    watch: CellWatch,
}

/// A wait on a slot's cell that owns its watch.
type CellWait = Pin<Box<dyn Future<Output = CellOutcome> + Send>>;

/// Resolves with the index and outcome of the first of `cells` that settled,
/// and drops it. Pending while none has.
fn next_settled(cells: &mut [Option<CellWait>]) -> impl Future<Output = (usize, CellOutcome)> + '_ {
    std::future::poll_fn(move |cx| {
        for (index, slot) in cells.iter_mut().enumerate() {
            if let Some(wait) = slot {
                if let Poll::Ready(outcome) = wait.as_mut().poll(cx) {
                    *slot = None;
                    return Poll::Ready((index, outcome));
                }
            }
        }
        Poll::Pending
    })
}

/// Waits for the given stops to settle and records the ends. The restart
/// plane is drained meanwhile: an attempt ordered to stop reports on it, and
/// an outcome that waited there for the supervisor to adopt it is adopted, so
/// its runner honors the order. Each wait ends with its cell's settlement
/// and, for an attempt, the drain of its own report, or the absence of any
/// restart-plane task working for it (its report was taken before the wait);
/// an Unresponsive verdict ends it at once. A factory drop panic a drained
/// attempt carries is kept in `drop_panic`.
async fn settle_children<A: Actor>(
    ctx: &mut ActorContext<A>,
    waits: Vec<StopWait>,
    stop_rx: &mut watch::Receiver<LaneState>,
    drop_panic: &mut Option<String>,
) -> Teardown {
    let supervisor = ctx.actor_id().clone();
    let mut targets: Vec<(ActorId, u64, bool)> = Vec::with_capacity(waits.len());
    let mut cells: Vec<Option<CellWait>> = Vec::with_capacity(waits.len());
    for wait in waits {
        let StopWait {
            id,
            token,
            attempt,
            mut watch,
        } = wait;
        targets.push((id, token, attempt));
        cells.push(Some(Box::pin(async move {
            await_cell(&mut watch, token).await
        })));
    }
    let mut settled: Vec<Option<CellOutcome>> = targets.iter().map(|_| None).collect();
    let mut reported = vec![false; targets.len()];
    let mut lane_open = true;
    if kill_stored(stop_rx) {
        return Teardown::Killed;
    }
    while let Some(sup) = ctx.supervision_mut() {
        // A target is done once its cell settled and, for an attempt, its own
        // report was drained, or no task of the restart plane works for it
        // any more (its report was taken before this wait: the final
        // collection takes it); an Unresponsive verdict ends it at once.
        let all_done = targets
            .iter()
            .zip(&settled)
            .all(|((id, token, attempt), outcome)| match outcome {
                Some(outcome) => {
                    !attempt
                        || matches!(outcome, CellOutcome::Unresponsive)
                        || !sup.task_owner.values().any(|(owner, incarnation, plane)| {
                            owner == id && incarnation == token && *plane == TaskPlane::Restart
                        })
                }
                None => false,
            });
        if all_done {
            break;
        }
        let restart_pending = !sup.restart_set.is_empty();
        let cells_pending = cells.iter().any(Option::is_some);
        tokio::select! {
            biased;
            // A `Kill` raised while the teardown waits turns the rest of it
            // into the `Kill` path. Only a stored `Kill` ends the wait: the
            // lane holds nothing else new once it is narrowed, and a version
            // the loop never consumed is consumed here.
            changed = stop_rx.changed(), if lane_open => {
                match changed {
                    Ok(()) if kill_stored(stop_rx) => return Teardown::Killed,
                    Ok(()) => {}
                    Err(_) => lane_open = false,
                }
            }
            (index, outcome) = next_settled(&mut cells), if cells_pending => {
                settled[index] = Some(outcome);
            }
            joined = sup.restart_set.join_next_with_id(), if restart_pending => {
                if let Some(joined) = joined {
                    if let Some((child, incarnation, adopted)) =
                        drain_restart_for_teardown(ctx, joined, drop_panic)
                    {
                        for (index, (id, token, attempt)) in targets.iter_mut().enumerate() {
                            if *id == child && *token == incarnation {
                                *attempt = false;
                                reported[index] = !adopted;
                            }
                        }
                    }
                }
            }
        }
    }
    for (index, outcome) in settled.into_iter().enumerate() {
        let Some(CellOutcome::Fate(fate)) = outcome else {
            continue;
        };
        if reported[index] {
            continue;
        }
        let (id, token, _) = &targets[index];
        if let Some(child) = ctx.supervision_ref().and_then(|sup| sup.registry.get(id)) {
            warn_unexpected_end(
                &supervisor,
                id,
                *token,
                &fate.reason,
                child.spec.restart_type,
                child.spec.shutdown,
            );
        }
    }
    Teardown::Done
}

/// How the teardown of a supervisor's children ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Teardown {
    /// Every child was settled (or left behind unresponsive).
    Done,
    /// A `Kill` reached the supervisor meanwhile: the rest of the teardown
    /// is the `Kill` path.
    Killed,
}

/// Carries out the teardown's action for one child and names the stop to
/// wait for: the incarnation whose end settles it, and whether that
/// incarnation is a restart attempt. A running child is ordered to stop, a
/// child whose stop is in progress is joined (never given a second ladder;
/// under `Infinity` a vetoable request is asked once more), and a restart
/// attempt is ordered to stop too - it cancels before its factory runs, or
/// stops its starting incarnation.
fn begin_child_stop(id: &ActorId, child: &ChildState, action: TeardownAction) -> Option<StopWait> {
    let (token, attempt) = match (action, child.lifecycle) {
        (TeardownAction::Order, ChildLifecycle::Running(n)) => {
            child.ctl.order_stop(child.spec.shutdown, true);
            (n, false)
        }
        (
            TeardownAction::Join { ask },
            ChildLifecycle::Running(n) | ChildLifecycle::Stopping { incarnation: n, .. },
        ) => {
            if ask {
                child.ctl.ask_again();
            }
            (n, false)
        }
        (TeardownAction::OrderAttempt { ask }, ChildLifecycle::Restarting { next, .. }) => {
            child.ctl.order_stop(child.spec.shutdown, false);
            if ask {
                child.ctl.ask_again();
            }
            (next, true)
        }
        _ => return None,
    };
    Some(StopWait {
        id: id.clone(),
        token,
        attempt,
        watch: child.ctl.watch(),
    })
}

/// Stops all children under their own `Shutdown` policies. The teardown takes
/// every occupant. Each child's runner (or attempt) raises the first signal,
/// escalates a `Timeout` to Kill at expiry and then to an abort, and records
/// the end; a populated fate implies the child task has fully terminated (the
/// runner only writes it after the child's own `JoinHandle` resolves, which
/// tokio guarantees only after every one of the child's own drops have
/// already run). A child that outlives its whole ladder carries the
/// Unresponsive verdict and is left behind instead of hanging the parent's
/// shutdown. No strategy, restart or `on_child_stopped` runs meanwhile. Never
/// reached when this task is itself dying with Kill - that path drops the
/// supervision state directly instead (see the `run_actor` cascade-inversion
/// comment). The child factories and the supervision state are dropped with
/// every drop caught; the first panic is left in `drop_panic`, as is the panic
/// of a factory's drop a restart attempt made when the factory's spec went
/// during its build.
///
/// A pool orders every child at once, in one step, and then waits for all of
/// them. Every other supervisor stops its children newest first, each after
/// the previous one settled.
async fn stop_all_children<A: Actor>(
    ctx: &mut ActorContext<A>,
    stop_rx: &mut watch::Receiver<LaneState>,
    drop_panic: &mut Option<String>,
) -> Teardown {
    let (strategy, order, factories) = match ctx.supervision_mut() {
        Some(sup) => {
            let factories = std::mem::take(&mut sup.restart_fns);
            sup.pending_group = None;
            sup.queued_triggers.clear();
            (
                sup.config.strategy,
                sup.registry
                    .iter()
                    .map(|child| child.id.clone())
                    .collect::<Vec<_>>(),
                factories,
            )
        }
        None => return Teardown::Done,
    };
    // The child factories are dropped one at a time, each drop caught.
    let supervisor = ctx.actor_id().clone();
    for (_, factory) in factories {
        keep_first_of(drop_panic, end_value(factory), &supervisor);
    }

    if strategy == RestartStrategy::SimpleOneForOne {
        let Some(sup) = ctx.supervision_ref() else {
            return Teardown::Done;
        };
        let children: Vec<(ActorId, rules::WalkChild)> = order
            .iter()
            .filter_map(|id| {
                sup.registry
                    .get(id)
                    .map(|child| (id.clone(), walk_child(child)))
            })
            .collect();
        let waits: Vec<StopWait> = rules::teardown_walk(strategy, &children)
            .into_iter()
            .filter_map(|(id, action)| {
                let child = sup.registry.get(&id)?;
                begin_child_stop(&id, child, action)
            })
            .collect();
        if settle_children(ctx, waits, stop_rx, drop_panic).await == Teardown::Killed {
            return Teardown::Killed;
        }
    } else {
        // Newest first.
        for id in order.into_iter().rev() {
            let Some(sup) = ctx.supervision_ref() else {
                return Teardown::Done;
            };
            let Some(child) = sup.registry.get(&id) else {
                continue;
            };
            #[cfg(test)]
            end_hooks::run(end_hooks::WALK, &supervisor);
            let walked = rules::teardown_walk(strategy, &[(id.clone(), walk_child(child))]);
            let Some((_, action)) = walked.into_iter().next() else {
                continue;
            };
            let Some(wait) = begin_child_stop(&id, child, action) else {
                continue;
            };
            if settle_children(ctx, vec![wait], stop_rx, drop_panic).await == Teardown::Killed {
                return Teardown::Killed;
            }
        }
    }
    if let Some(sup) = ctx.take_supervision() {
        keep_first_of(drop_panic, end_supervision(sup, &supervisor), &supervisor);
    }
    Teardown::Done
}

/// The reports of incarnation ends the supervisor's message loop left
/// pending, collected for their own deliveries.
struct Collected {
    report: ChildStoppedInternal,
    name: Option<String>,
}

/// What the final deliveries ended with.
enum FinalOutcome {
    /// Every delivery ran. `pending` is the exit reason a failed delivery
    /// makes of the supervisor, applied when `on_stopped` returns.
    Done { pending: Option<StopReason> },
    /// A `Kill` reached the supervisor: the rest of the deliveries and
    /// `on_stopped` are skipped.
    Killed,
}

/// Whether the stop lane holds a `Kill`, read without consuming it.
fn kill_stored(stop_rx: &watch::Receiver<LaneState>) -> bool {
    matches!(stop_rx.borrow().reason, Some(StopReason::Kill))
}

/// Pushes one collected report, with the child's name as the ledger (or a
/// pending snapshot of the end) holds it, and records the end in the ledger.
fn collect_report(
    sup: &mut SupervisionState,
    out: &mut Vec<Collected>,
    report: ChildStoppedInternal,
) {
    let name = sup
        .registry
        .get(&report.child_id)
        .and_then(|child| child.name.clone())
        .or_else(|| {
            sup.pending_manual_events
                .get(&(report.child_id.clone(), report.incarnation))
                .and_then(|event| event.child_name.clone())
        })
        .or_else(|| {
            sup.owed_names
                .remove(&(report.child_id.clone(), report.incarnation))
        });
    record_collected_end(sup, &report.child_id, report.incarnation);
    out.push(Collected { report, name });
}

/// Collects, delivering nothing yet, every report of an incarnation's end
/// that was still pending when the supervisor's message loop ended.
///
/// The runners of the incarnations whose tasks had already ended, and the
/// attempts whose incarnations had, are awaited first: a runner writes its
/// fate and returns after the task published `Stopped`, so reading the
/// planes at once would miss a child that ended just before the loop did.
/// The set collected is fixed by this collection: an incarnation whose end
/// it does not see is the teardown's. Then, in this order: the worklist a
/// completing group left, the deferred triggers, the restart plane (an
/// adopted attempt is adopted, nothing is delivered for it) and the death
/// plane, each read until empty. Each collected report is recorded as the
/// teardown records a drain: the ledger reaches `Down` with nothing owed, a
/// snapshot of the end is dropped, a barrier entry is released. No strategy,
/// charge, restart or group step runs.
async fn collect_final<A: Actor>(
    ctx: &mut ActorContext<A>,
    drop_panic: &mut Option<String>,
) -> Vec<Collected> {
    let supervisor = ctx.actor_id().clone();
    let mut out: Vec<Collected> = Vec::new();
    let Some(sup) = ctx.supervision_ref() else {
        return out;
    };
    let (mut runners, mut attempts) = sup.final_await_sets();
    // Runners of adopted attempts whose incarnation ended in transit.
    let mut adopted: std::collections::HashSet<tokio::task::Id> = Default::default();

    // The runners of the incarnations that ended.
    while !runners.is_empty() {
        let Some(sup) = ctx.supervision_mut() else {
            break;
        };
        match sup.death_set.join_next_with_id().await {
            None => break,
            Some(Ok((task, (child_id, reason, incarnation)))) => {
                sup.task_owner.remove(&task);
                runners.remove(&task);
                collect_report(
                    sup,
                    &mut out,
                    ChildStoppedInternal {
                        child_id,
                        reason,
                        incarnation,
                    },
                );
            }
            Some(Err(err)) => {
                runners.remove(&err.id());
                if let Some(replacement) = ctx.lost_death_task(err) {
                    runners.insert(replacement);
                }
            }
        }
    }

    // The attempts whose incarnations ended.
    while !attempts.is_empty() {
        let Some(sup) = ctx.supervision_mut() else {
            break;
        };
        match sup.restart_set.join_next_with_id().await {
            None => break,
            Some(Ok((task, outcome))) => {
                sup.task_owner.remove(&task);
                attempts.remove(&task);
                collect_restart_outcome(
                    sup,
                    &supervisor,
                    &mut out,
                    &mut adopted,
                    outcome,
                    drop_panic,
                );
            }
            Some(Err(err)) => {
                attempts.remove(&err.id());
                let (outcome, replacement) = ctx.lost_restart_task(err);
                if let Some(replacement) = replacement {
                    adopted.insert(replacement);
                }
                if let (Some(outcome), Some(sup)) = (outcome, ctx.supervision_mut()) {
                    collect_restart_outcome(
                        sup,
                        &supervisor,
                        &mut out,
                        &mut adopted,
                        outcome,
                        drop_panic,
                    );
                }
            }
        }
    }

    // The worklist a completing group left, then the deferred triggers.
    if let Some(sup) = ctx.supervision_mut() {
        let owed: Vec<ChildStoppedInternal> = sup.owed.drain(..).collect();
        for report in owed {
            collect_report(sup, &mut out, report);
        }
        let queued: Vec<ChildStoppedInternal> = sup.queued_triggers.drain(..).collect();
        for report in queued {
            collect_report(sup, &mut out, report);
        }
    }

    // The restart plane.
    while let Some(sup) = ctx.supervision_mut() {
        match sup.restart_set.try_join_next_with_id() {
            None => break,
            Some(Ok((task, outcome))) => {
                sup.task_owner.remove(&task);
                collect_restart_outcome(
                    sup,
                    &supervisor,
                    &mut out,
                    &mut adopted,
                    outcome,
                    drop_panic,
                );
            }
            Some(Err(err)) => {
                let (outcome, replacement) = ctx.lost_restart_task(err);
                if let Some(replacement) = replacement {
                    adopted.insert(replacement);
                }
                if let (Some(outcome), Some(sup)) = (outcome, ctx.supervision_mut()) {
                    collect_restart_outcome(
                        sup,
                        &supervisor,
                        &mut out,
                        &mut adopted,
                        outcome,
                        drop_panic,
                    );
                }
            }
        }
    }

    // The death plane: the runners the adoptions above spawned for
    // incarnations that ended in transit first, then everything it holds.
    loop {
        while !adopted.is_empty() {
            let Some(sup) = ctx.supervision_mut() else {
                break;
            };
            match sup.death_set.join_next_with_id().await {
                None => {
                    adopted.clear();
                }
                Some(Ok((task, (child_id, reason, incarnation)))) => {
                    sup.task_owner.remove(&task);
                    adopted.remove(&task);
                    collect_report(
                        sup,
                        &mut out,
                        ChildStoppedInternal {
                            child_id,
                            reason,
                            incarnation,
                        },
                    );
                }
                Some(Err(err)) => {
                    adopted.remove(&err.id());
                    if let Some(replacement) = ctx.lost_death_task(err) {
                        adopted.insert(replacement);
                    }
                }
            }
        }
        let Some(sup) = ctx.supervision_mut() else {
            break;
        };
        match sup.death_set.try_join_next_with_id() {
            None => break,
            Some(Ok((task, (child_id, reason, incarnation)))) => {
                sup.task_owner.remove(&task);
                collect_report(
                    sup,
                    &mut out,
                    ChildStoppedInternal {
                        child_id,
                        reason,
                        incarnation,
                    },
                );
            }
            Some(Err(err)) => {
                if let Some(replacement) = ctx.lost_death_task(err) {
                    adopted.insert(replacement);
                }
            }
        }
    }

    // Nothing of the group is left to do, and `children()` inside a final
    // delivery reads no member as awaiting a restart that will not be made.
    if let Some(sup) = ctx.supervision_mut() {
        sup.pending_group = None;
    }
    out
}

/// Records one restart-plane outcome of the final collection: a failed or
/// stopped attempt is collected for its own delivery, an adopted one becomes
/// the child's running incarnation (its runner is awaited with the death
/// plane when its incarnation had ended in transit), a cancelled one is only
/// recorded, and a factory drop panic it carries is kept in `drop_panic`
/// with the teardown's, for the end to apply before the reason is final.
fn collect_restart_outcome(
    sup: &mut SupervisionState,
    supervisor: &ActorId,
    out: &mut Vec<Collected>,
    adopted: &mut std::collections::HashSet<tokio::task::Id>,
    outcome: RestartOutcome,
    drop_panic: &mut Option<String>,
) {
    let (child_id, incarnation, runner, reason) =
        record_restart_outcome(sup, supervisor, outcome, false, drop_panic);
    if let Some(runner) = runner {
        if runner.ended {
            adopted.insert(runner.task);
        }
    }
    if let Some(reason) = reason {
        collect_report(
            sup,
            out,
            ChildStoppedInternal {
                child_id,
                reason,
                incarnation,
            },
        );
    }
}

/// The final deliveries of a supervisor whose message loop ended for any
/// reason but `Kill`, between the loop and `on_stopped`: every report still
/// pending is delivered once, with `SupervisorStopping`, the supervisor's
/// status reading `Stopping`, and no restart or charge. The stop lane is
/// read before each delivery; a `Kill` that arrived ends the drain at once.
/// A delivery that returns an `Err` or panics makes the supervisor's exit
/// reason a `Failure` once `on_stopped` returns, and the remaining
/// deliveries still run. A factory drop panic the collection meets is kept
/// in `drop_panic`, the teardown's.
async fn final_drain<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    stop_rx: &watch::Receiver<LaneState>,
    accepted: &StopReason,
    drop_panic: &mut Option<String>,
) -> FinalOutcome {
    let collected = collect_final(ctx, drop_panic).await;
    // The waiters on the status plane run before the first delivery.
    tokio::task::yield_now().await;
    let clock = ctx.turn_clock();
    let mut pending: Option<StopReason> = None;
    for Collected { report, name } in collected {
        if kill_stored(stop_rx) {
            return FinalOutcome::Killed;
        }
        let event = ChildEvent {
            child_id: report.child_id,
            child_name: name,
            reason: report.reason,
            action: SupervisionAction::SupervisorStopping,
        };
        if let Some(sup) = ctx.supervision_mut() {
            sup.final_delivery = true;
        }
        clock.begin();
        let result = catch_callback(actor.on_child_stopped(&event, ctx)).await;
        clock.end();
        if let Some(sup) = ctx.supervision_mut() {
            sup.final_delivery = false;
        }
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                ctx.record_failure(err.clone());
                pending = Some(StopReason::Failure(err));
            }
            Err(payload) => {
                let err = ActorError::Panic(payload_into_string(payload));
                ctx.record_failure(err.clone());
                // A panic never masks an earlier failure.
                let failed = matches!(accepted, StopReason::Failure(_))
                    || matches!(pending, Some(StopReason::Failure(_)));
                if !failed {
                    pending = Some(StopReason::Failure(err));
                }
            }
        }
    }
    FinalOutcome::Done { pending }
}

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

/// The far-future horizon `saturating_deadline` caps every input at: the
/// same 30-year horizon Tokio itself uses for an overflowing timeout.
pub(crate) const FAR_FUTURE: Duration = Duration::from_secs(86_400 * 365 * 30);

/// Adds `dur` to `base`, capping the addition at [`FAR_FUTURE`] instead of
/// letting it approach the edge of the representable `Instant` range.
///
/// A plain `base.checked_add(dur)` is not enough on its own: even when it
/// succeeds, the timer that later consumes this deadline rounds it up to
/// the next millisecond with its own unchecked addition, which panics if
/// the deadline this function returned was already within the last
/// millisecond of what `Instant` can represent. Capping `dur` at
/// `FAR_FUTURE` before adding it keeps every returned deadline at most 30
/// years past `base` - which any caller-facing timer treats as never
/// firing - so it is always safely convertible by the timer, on every
/// input, not just the ones a plain `checked_add` already rejects.
///
/// Used wherever a deadline `Instant` is computed from `Instant::now()` plus
/// a caller-controlled duration: `send_timeout`, system shutdown deadlines,
/// and timer scheduling. A caller-controlled [`Duration`] handed directly to
/// `tokio::time::sleep`/`timeout` instead - never turned into a deadline
/// `Instant` here - goes through [`bounded_wait`] instead.
pub(crate) fn saturating_deadline(base: Instant, dur: Duration) -> Instant {
    base.checked_add(dur.min(FAR_FUTURE)).unwrap_or(base)
}

/// Caps `dur` at [`FAR_FUTURE`]: a wait of at most 30 years, which any
/// caller-facing timer treats as never firing, and always safely
/// convertible by `tokio::time::sleep`/`timeout` on every input, not just
/// the ones already within the representable `Instant` range.
///
/// Used wherever a caller-controlled duration is handed directly to
/// `tokio::time::sleep`/`timeout` instead of first being turned into a
/// deadline `Instant` via [`saturating_deadline`]: `wait_terminal`,
/// `SpawnBuilder::start_timeout`, a child's restart `start_timeout`, and
/// every `Shutdown::Timeout` wait.
pub(crate) fn bounded_wait(dur: Duration) -> Duration {
    dur.min(FAR_FUTURE)
}

fn build_status_info<A: Actor>(ctx: &ActorContext<A>) -> ActorStatusInfo {
    let (child_count, name) = ctx
        .supervision_ref()
        .map(|sup| (sup.registry.len(), ctx.actor_name().cloned()))
        .unwrap_or((0, ctx.actor_name().cloned()));
    let handle = ctx.self_handle();

    ActorStatusInfo {
        id: ctx.actor_id().clone(),
        name,
        status: ctx.status(),
        mailbox_len: handle.mailbox_len(),
        mailbox_capacity: handle.mailbox_capacity(),
        priority_len: handle.priority_len(),
        priority_capacity: handle.priority_capacity(),
        child_count,
        timer_count: ctx.active_timer_count(),
        stream_count: ctx.active_stream_count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn saturating_deadline_matches_plain_addition_within_range() {
        let base = Instant::now();
        let dur = Duration::from_secs(5);
        assert_eq!(saturating_deadline(base, dur), base + dur);
    }

    #[tokio::test]
    async fn saturating_deadline_saturates_instead_of_panicking() {
        let base = Instant::now();
        // A plain `base + Duration::MAX` panics; the helper must not.
        let deadline = saturating_deadline(base, Duration::MAX);
        assert!(
            deadline > base,
            "an overflowing duration must still saturate to a future instant"
        );
    }

    #[tokio::test]
    async fn saturating_deadline_zero_duration_is_a_no_op() {
        let base = Instant::now();
        assert_eq!(saturating_deadline(base, Duration::ZERO), base);
    }

    #[tokio::test]
    async fn saturating_deadline_always_leaves_timer_headroom() {
        let base = Instant::now();

        // A deadline that leaves no room for the timer's own rounding-up
        // addition is unsafe regardless of how it was produced: assert the
        // margin directly instead of trusting any particular code path.
        let assert_has_headroom = |dur: Duration| {
            let deadline = saturating_deadline(base, dur);
            assert!(
                deadline.checked_add(Duration::from_secs(1)).is_some(),
                "deadline for {dur:?} left less than a second of timer headroom"
            );
        };

        assert_has_headroom(Duration::MAX);
        assert_has_headroom(Duration::from_secs(u64::MAX));

        // The largest duration `base.checked_add` still accepts is the exact
        // boundary a plain `checked_add`-based implementation would return
        // unchanged: find it by binary search down to 1ns precision, so this
        // probes the edge directly instead of only far-overflowing inputs.
        let mut lo = Duration::ZERO;
        let mut hi = Duration::from_secs(u64::MAX / 2);
        while hi - lo > Duration::from_nanos(1) {
            let mid = lo + (hi - lo) / 2;
            if base.checked_add(mid).is_some() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        assert_has_headroom(lo);
    }

    #[tokio::test]
    async fn saturating_deadline_caps_at_thirty_years() {
        let base = Instant::now();
        assert_eq!(saturating_deadline(base, Duration::MAX), base + FAR_FUTURE);
        assert_eq!(
            saturating_deadline(base, Duration::from_secs(u64::MAX)),
            base + FAR_FUTURE
        );
    }

    // - route_report ---------------------------------------------------------

    use crate::actor::supervision::{slot_pair, ChildSpec};

    /// A supervisor whose OneForAll chain pends over `queue`, with the
    /// children `lives` names in the registry and a budget of one restart.
    fn a_chain(queue: &[&str], lives: &[(&str, ChildLifecycle)]) -> SupervisionState {
        let mut sup = SupervisionState::new(SupervisionConfig {
            strategy: RestartStrategy::OneForAll,
            max_restarts: 1,
            restart_window: Duration::from_secs(60),
        });
        for (id, life) in lives {
            let (ctl, _io) = slot_pair(Default::default());
            sup.registry.register(ChildState {
                id: ActorId::from(*id),
                name: Some((*id).to_string()),
                spec: ChildSpec {
                    restart_type: RestartType::Permanent,
                    shutdown: Shutdown::default(),
                    start_timeout: None,
                    kill_grace: DEFAULT_KILL_GRACE,
                    is_supervisor: false,
                },
                ctl,
                lifecycle: *life,
                origin: Origin::Chain,
                predecessor: None,
            });
        }
        sup.pending_group = Some(GroupPhase::Restarting {
            queue: queue.iter().map(|id| ActorId::from(*id)).collect(),
            overrides: HashMap::new(),
            started: HashMap::new(),
            scope: GroupScope::default(),
        });
        sup
    }

    fn down(n: u64) -> ChildLifecycle {
        ChildLifecycle::Down {
            incarnation: n,
            pending: DownPending::None,
        }
    }

    fn crash_of(id: &str, incarnation: u64) -> ChildStoppedInternal {
        ChildStoppedInternal {
            child_id: ActorId::from(id),
            reason: StopReason::Failure(ActorError::user("crashed")),
            incarnation,
        }
    }

    #[test]
    fn the_end_of_a_queued_member_behind_the_front_is_the_groups_and_charges_nothing() {
        // The front is down; the second member runs an incarnation the chain
        // did not start. Its end belongs to the group: it is delivered as the
        // restart, the member reads down, the chain does not advance and
        // nothing is charged.
        let mut sup = a_chain(
            &["front", "second"],
            &[("front", down(1)), ("second", ChildLifecycle::Running(5))],
        );
        let mut steps = GroupSteps::default();
        let routed = route_report(&mut sup, &crash_of("second", 5), &mut steps);
        assert!(matches!(
            routed,
            Routed::Deliver(SupervisionAction::RestartInitiated)
        ));
        assert!(!steps.advance_chain);
        assert!(!steps.manual_override);
        assert_eq!(
            sup.registry
                .get(&ActorId::from("second"))
                .unwrap()
                .lifecycle,
            ChildLifecycle::Down {
                incarnation: 5,
                pending: DownPending::None
            }
        );
        assert!(
            sup.budget.check_and_record(),
            "the budget of one was not charged"
        );
    }

    #[test]
    fn the_end_of_the_front_member_advances_the_chain() {
        // The front itself runs an incarnation the chain did not start.
        let mut sup = a_chain(&["front"], &[("front", ChildLifecycle::Running(5))]);
        let mut steps = GroupSteps::default();
        let routed = route_report(&mut sup, &crash_of("front", 5), &mut steps);
        assert!(matches!(
            routed,
            Routed::Deliver(SupervisionAction::RestartInitiated)
        ));
        assert!(steps.advance_chain);
        assert_eq!(
            sup.registry.get(&ActorId::from("front")).unwrap().lifecycle,
            ChildLifecycle::Down {
                incarnation: 5,
                pending: DownPending::None
            }
        );
    }
}

#[cfg(test)]
mod task_marker_tests {
    use super::*;

    /// A task whose own poll unwinds past every catch, while it holds an
    /// abort guard: the guard drops inside that unwind and writes the panic,
    /// and the runner's reading of the join (`joined_reason`) is the plane's
    /// reason, one value with one text.
    async fn an_unwind_of_the_task_s_own_poll_reads_one_panic() {
        let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
        let marker = TaskMarker::default();
        let guard = AbortReason::new(status_tx.clone(), marker.clone(), ActorId::from("unwound"));
        let join = tokio::spawn(MarkedRun::new(marker, async move {
            let _guard = guard;
            tokio::task::yield_now().await;
            if std::hint::black_box(true) {
                panic!("the crate's own code panics in the task's poll");
            }
            StopReason::Graceful
        }));
        let res = join.await;
        assert!(res.as_ref().is_err_and(|err| err.is_panic()));
        let plane = status_rx.borrow().reason.clone();
        let want = StopReason::Failure(ActorError::Panic(UNCAUGHT_PANIC.to_string()));
        assert_eq!(plane, Some(want.clone()), "the plane reads the panic");
        assert_eq!(
            crate::actor::supervision::joined_reason(res, plane, None),
            want,
            "the runner reads the plane's reason"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_unwind_of_the_task_s_own_poll_reads_one_panic_current_thread() {
        an_unwind_of_the_task_s_own_poll_reads_one_panic().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unwind_of_the_task_s_own_poll_reads_one_panic_multi_thread() {
        an_unwind_of_the_task_s_own_poll_reads_one_panic().await;
    }

    /// The guard of a task aborted while suspended writes `Kill`, and the
    /// runner reads `Kill`.
    async fn an_abort_of_a_suspended_task_reads_kill() {
        let (status_tx, status_rx) = watch::channel(StatusValue::new(ActorStatus::Running));
        let marker = TaskMarker::default();
        let guard = AbortReason::new(status_tx.clone(), marker.clone(), ActorId::from("aborted"));
        let join = tokio::spawn(MarkedRun::new(marker, async move {
            let _guard = guard;
            std::future::pending::<()>().await;
            StopReason::Graceful
        }));
        tokio::task::yield_now().await;
        join.abort();
        let res = join.await;
        assert!(res.as_ref().is_err_and(|err| err.is_cancelled()));
        let plane = status_rx.borrow().reason.clone();
        assert_eq!(plane, Some(StopReason::Kill));
        assert_eq!(
            crate::actor::supervision::joined_reason(res, plane, None),
            StopReason::Kill
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_abort_of_a_suspended_task_reads_kill_current_thread() {
        an_abort_of_a_suspended_task_reads_kill().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abort_of_a_suspended_task_reads_kill_multi_thread() {
        an_abort_of_a_suspended_task_reads_kill().await;
    }
}
