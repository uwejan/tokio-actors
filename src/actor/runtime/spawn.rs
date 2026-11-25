//! The spawn path: checks, registration, the parts of a new incarnation and its task.

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio::time::Duration;

use super::{
    end_context, end_value, registry_parent, run_actor, AbortReason, ActorConfig, CaughtDrop,
    MarkedRun, Owner, QueueSlot, RunParts,
};
use crate::actor::context::ActorContext;
use crate::actor::handle::{ActorHandle, LaneFlags};
use crate::actor::incarnation::turn_clock::TurnGuard;
use crate::actor::incarnation::{
    IncarnationRef, StatusTx, StopLane, SubtreeHold, SubtreeLink, SubtreeWatch, SystemPlane,
    TurnClock,
};
use crate::actor::panic::AnswerHold;
#[cfg(test)]
use crate::actor::runtime::end_hooks;
use crate::actor::supervision::{
    run_ladder, ChildLinkGuard, DeathOutcome, RunnerEnd, SlotIo, SupervisionState,
};
use crate::actor::sys::SysOptions;
use crate::actor::Actor;
use crate::error::{ActorError, SpawnError};
use crate::system::{ActorSystem, RegistryGuard};
use crate::telemetry::{ActorSpan, Birth, EventKind, Identity, SpawnTraced, TelemetryEvent};
use crate::types::{ActorId, ChildFate, RestartStrategy, StopReason};

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
    name: Option<Arc<str>>,
    system: Option<Arc<ActorSystem>>,
    kill_grace: Duration,
    sys: SysOptions,
) -> Result<ActorHandle<A>, SpawnError> {
    // Only reachable from the top-level `.detached()` spawn path: always a root.
    let (handle, _join, _subtree) = spawn_actor_in(
        id.into(),
        actor,
        config.into(),
        name,
        system,
        None,
        true,
        kill_grace,
        None,
        None,
        Birth::root(),
        sys,
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
/// shutdown walks the roots; a root's own supervision cascade takes its
/// children down in turn.
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

/// What [`spawn_actor_parts_in`] returns: a [`SpawnOutcome`]'s parts with a
/// sender of the status plane when the caller asked to keep one.
type SpawnParts<A> = (
    ActorHandle<A>,
    JoinHandle<StopReason>,
    Option<StatusTx>,
    SubtreeWatch,
);

/// Queues the event of a refused spawn and returns the refusal. The system is
/// the one the spawn names, or the default system when it exists; a spawn
/// that names none and finds no default system has no sink to tell. Called
/// with no registry guard held, once per refused spawn.
pub(crate) fn refused(
    system: Option<&Arc<ActorSystem>>,
    name: Option<&str>,
    error: SpawnError,
) -> SpawnError {
    let default;
    let system = match system {
        Some(system) => Some(system),
        None => {
            default = ActorSystem::get_named("default");
            default.as_ref()
        }
    };
    if let Some(system) = system {
        system
            .telemetry_mode()
            .push(EventKind::SpawnRefused, || TelemetryEvent::SpawnRefused {
                name: name.map(|name| std::borrow::Cow::Owned(name.to_string())),
                error: error.clone(),
            });
    }
    error
}

/// Queues the event of a spawn the registry accepted, before the actor's task
/// exists, so no event the task raises reaches a sink ahead of it.
#[inline]
fn emit_actor_spawned(
    system: &ActorSystem,
    record: &IncarnationRef,
    id: &ActorId,
    name: Option<&str>,
    mailbox_capacity: usize,
    priority_capacity: Option<usize>,
) {
    if !system.telemetry_mode().armed(EventKind::ActorSpawned) {
        return;
    }
    emit_actor_spawned_armed(
        system,
        record,
        id,
        name,
        mailbox_capacity,
        priority_capacity,
    );
}

/// The spawn event of a system whose sinks take it: kept out of line, so a
/// spawn that nobody listens to tests one word and goes on.
#[inline(never)]
fn emit_actor_spawned_armed(
    system: &ActorSystem,
    record: &IncarnationRef,
    id: &ActorId,
    name: Option<&str>,
    mailbox_capacity: usize,
    priority_capacity: Option<usize>,
) {
    crate::telemetry::with_mode(record, Some(system), |mode| {
        if !mode.armed(EventKind::ActorSpawned) {
            return;
        }
        let Some(identity) = crate::telemetry::identity_for(mode, record, Some(system), Some(id))
        else {
            mode.pump.count_dropped();
            return;
        };
        mode.push_owned(&identity, EventKind::ActorSpawned, || {
            TelemetryEvent::ActorSpawned {
                actor: identity.actor().clone(),
                name: name.map(|name| std::borrow::Cow::Owned(name.to_string())),
                mailbox_capacity,
                priority_capacity,
            }
        });
    });
}

/// The refusal a mailbox configuration earns before anything is created.
fn mailbox_config_error(config: &ActorConfig) -> Result<(), SpawnError> {
    if config.mailbox.capacity == 0 || config.mailbox.priority_capacity == Some(0) {
        return Err(SpawnError::ZeroMailboxCapacity);
    }
    Ok(())
}

/// The refusal a supervision setup earns before anything is created: a
/// supervisor that sets `auto_shutdown` and uses `SimpleOneForOne`, which
/// Erlang/OTP refuses too.
pub(crate) fn supervision_setup_error(config: &ActorConfig) -> Result<(), SpawnError> {
    match &config.supervision {
        Some(sup)
            if sup.auto_shutdown != crate::actor::supervision::AutoShutdown::Never
                && sup.strategy == RestartStrategy::SimpleOneForOne =>
        {
            Err(SpawnError::InvalidSupervision(
                "auto_shutdown with simple_one_for_one",
            ))
        }
        _ => Ok(()),
    }
}

/// What a spawn answers before it creates or claims anything, in the order
/// the spawn itself answers it: a zero mailbox capacity, then (with a runtime
/// current) a system that is not taking registrations, then a name that
/// is already taken. Reads the registry and the system phase without
/// claiming. A spawn that must also check the calling runtime's timers calls
/// this first, so that what needs no timer is answered as it is without one.
pub(crate) fn spawn_precheck(
    config: &ActorConfig,
    system: Option<&Arc<ActorSystem>>,
    name: Option<&str>,
) -> Result<(), SpawnError> {
    mailbox_config_error(config).map_err(|err| refused(system, name, err))?;
    supervision_setup_error(config).map_err(|err| refused(system, name, err))?;
    if Handle::try_current().is_err() {
        return Ok(());
    }
    // A default system nothing has created holds no name and takes spawns.
    let target = match system {
        Some(system) => Some(system.clone()),
        None => ActorSystem::get_named("default"),
    };
    match target {
        Some(target) => target
            .precheck_spawn(name)
            .map_err(|err| refused(Some(&target), name, err)),
        None => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_actor<A: Actor>(
    id: ActorId,
    actor: A,
    config: ActorConfig,
    name: Option<Arc<str>>,
    system: Option<Arc<ActorSystem>>,
    ack: Option<oneshot::Sender<Result<(), ActorError>>>,
    is_root: bool,
    kill_grace: Duration,
    subtree: Option<SubtreeLink>,
) -> Result<SpawnOutcome<A>, SpawnError> {
    spawn_actor_in(
        id,
        actor,
        config,
        name,
        system,
        ack,
        is_root,
        kill_grace,
        subtree,
        None,
        Birth::root(),
        SysOptions::default(),
    )
}

/// [`spawn_actor`] on the runtime behind `placement` instead of the current
/// one, which every task, timer and registry entry of the actor then uses.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_actor_in<A: Actor>(
    id: ActorId,
    actor: A,
    config: ActorConfig,
    name: Option<Arc<str>>,
    system: Option<Arc<ActorSystem>>,
    ack: Option<oneshot::Sender<Result<(), ActorError>>>,
    is_root: bool,
    kill_grace: Duration,
    subtree: Option<SubtreeLink>,
    placement: Option<&Handle>,
    birth: Birth<'_>,
    sys: SysOptions,
) -> Result<SpawnOutcome<A>, SpawnError> {
    let (handle, join, _, subtree) = spawn_actor_parts_in(
        id, actor, config, name, system, ack, is_root, kill_grace, subtree, placement, false,
        birth, sys,
    )?;
    Ok((handle, join, subtree))
}

/// [`spawn_actor`], also returning a sender of the actor's status plane:
/// held by a caller, it keeps the plane open past the actor's own senders.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_actor_parts<A: Actor>(
    id: ActorId,
    actor: A,
    config: ActorConfig,
    name: Option<Arc<str>>,
    system: Option<Arc<ActorSystem>>,
    ack: Option<oneshot::Sender<Result<(), ActorError>>>,
    is_root: bool,
    kill_grace: Duration,
    subtree: Option<SubtreeLink>,
) -> Result<
    (
        ActorHandle<A>,
        JoinHandle<StopReason>,
        StatusTx,
        SubtreeWatch,
    ),
    SpawnError,
> {
    let (handle, join, status, subtree) = spawn_actor_parts_in(
        id,
        actor,
        config,
        name,
        system,
        ack,
        is_root,
        kill_grace,
        subtree,
        None,
        true,
        Birth::root(),
        SysOptions::default(),
    )?;
    let status = status.expect("a spawn asked to keep a sender returns one");
    Ok((handle, join, status, subtree))
}

/// [`spawn_actor`] on the runtime behind `placement`, the current one when it
/// is `None`, returning a sender of the actor's status plane when
/// `keep_status` is set.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_actor_parts_in<A: Actor>(
    id: ActorId,
    actor: A,
    config: ActorConfig,
    name: Option<Arc<str>>,
    system: Option<Arc<ActorSystem>>,
    ack: Option<oneshot::Sender<Result<(), ActorError>>>,
    is_root: bool,
    kill_grace: Duration,
    subtree: Option<SubtreeLink>,
    placement: Option<&Handle>,
    keep_status: bool,
    birth: Birth<'_>,
    sys: SysOptions,
) -> Result<SpawnParts<A>, SpawnError> {
    // A named actor's id is its name: the registry finds it by either.
    debug_assert!(name.as_deref().is_none_or(|name| name == id.as_str()));
    mailbox_config_error(&config).map_err(|err| refused(system.as_ref(), name.as_deref(), err))?;
    supervision_setup_error(&config)
        .map_err(|err| refused(system.as_ref(), name.as_deref(), err))?;
    let priority_by_alias = config.mailbox.priority_by_alias;
    let mailbox_capacity = config.mailbox.capacity;
    // Clamped, not rejected: a capacity above what the channel
    // implementation can represent is over-provisioning, not a
    // configuration error, so `usize::MAX` stays a valid "as large as this
    // can go" shorthand instead of panicking inside `mpsc::channel`.
    let mailbox_capacity = mailbox_capacity.min(tokio::sync::Semaphore::MAX_PERMITS);
    let priority_capacity = config
        .mailbox
        .priority_capacity
        .map(|cap| cap.min(tokio::sync::Semaphore::MAX_PERMITS));
    let handle = match placement {
        Some(placed) => placed.clone(),
        None => Handle::try_current()
            .map_err(|_| refused(system.as_ref(), name.as_deref(), SpawnError::MissingRuntime))?,
    };
    // What the mailbox and the run loop need to know about the runtime, read
    // once here from the handle the task is spawned on, which is the placement
    // runtime when the spawn names one: how many worker threads it has and
    // whether it is a current-thread runtime.
    let workers = handle.metrics().num_workers();
    let same_thread = matches!(
        handle.runtime_flavor(),
        tokio::runtime::RuntimeFlavor::CurrentThread
    );

    // Every spawn always resolves to a concrete system (bare anonymous
    // spawns join `ActorSystem::default()` as roots, same as named spawns
    // already did) - resolved up front so the handle can carry the system's
    // stable identity, used by `ActorHandle`'s `Eq`/`Hash` alongside its
    // `ActorId`.
    let target_system = system.clone().unwrap_or_else(ActorSystem::default);

    // The turn clock needs only the mode, so it is built ahead of the
    // mailbox. It is owned by the run loop (the writer) and the registry entry
    // (the reader) only, never by `ActorHandle`, so cloning a handle never
    // touches it.
    //
    // Every incarnation has a number: a supervisor draws a child's, and a
    // root draws its own here. It is the registration's sequence, so the
    // registry entry knows it for an event raised later, and it names the
    // incarnation in the wait graph through the clock's id.
    let incarnation = birth
        .incarnation
        .unwrap_or_else(|| target_system.next_incarnation());
    let turn_clock = TurnClock::with_id(target_system.telemetry_mode().clone(), incarnation);
    // The record names the system before the incarnation is registered: every
    // handle reaches its system through it, by identity. It is made ahead of
    // the mailbox, which keeps it to reach the actor's system and identity.
    let (stop_lane, stop_rx) = StopLane::new_in(
        target_system.telemetry_mode().clone(),
        crate::types::Incarnation::from_token(incarnation),
    );
    let (status_tx, status_rx) = stop_lane.abort_record().status_channel();
    let abort_record = stop_lane.abort_record().clone();

    let (tx, rx) = crate::actor::mailbox::channel(
        mailbox_capacity,
        crate::actor::mailbox::lanes_for(workers),
        Some(id.who()),
        Some(abort_record.clone()),
    );
    // State access rides the system channel and is decided by the block's
    // bits, which are written only when one is set: an actor that uses
    // nothing of the `sys` bundle has no channel, no field and no bit for it.
    let sensitive = sys.effective_sensitive::<A>();
    #[cfg(test)]
    crate::actor::sys_loop::starts::record(&id, sys);
    let sys_bits = (u8::from(sys.allow_state_access) * crate::actor::incarnation::SYS_ALLOW_STATE)
        | (u8::from(sensitive) * crate::actor::incarnation::SYS_SENSITIVE)
        | (u8::from(!is_root) * crate::actor::incarnation::SYS_CHILD);
    if sys_bits != 0 {
        abort_record.set_sys_bits(sys_bits);
    }
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
    // The span is the task's own: it is built here, moved into the task's
    // wrapper below and closed with the task. Its incarnation number and
    // system name are drawn only when a subscriber wants the span.
    let mode = target_system.telemetry_mode();
    let span = ActorSpan::for_spawn(
        &id,
        name.as_deref(),
        || incarnation,
        || target_system.name(),
    );
    // The identity is recorded before the registry entry exists, so no lookup
    // of the new actor finds a record without one, and only when a sink takes
    // events or a subscriber wants the span or the identity fields of the
    // actor's log statements: an actor spawned with none of them carries
    // nothing.
    // A sensitive actor always has one, so its log lines can tell it apart.
    // Turn tracking and wait detection count as taking events here: their
    // sites look identities up without an id.
    let bound = mode.word() & crate::telemetry::RESOLVING != 0
        || span.is_some()
        || ActorSpan::log_interest()
        || sensitive;
    if bound {
        let parent = birth.parent.map(|(parent, seq)| {
            target_system
                .resolve_identity_of(parent, seq)
                .unwrap_or_else(|| Arc::new(Identity::unknown(parent.clone())))
        });
        let born = mode
            .armed(EventKind::ActorEnded)
            .then(tokio::time::Instant::now);
        mode.bind(
            abort_record.key(),
            Arc::new(
                Identity::new(id.clone(), incarnation, parent, name.clone(), born)
                    .with_type(std::any::type_name::<A>())
                    .with_sensitive(sensitive),
            ),
        );
    }
    let abort_incarnation = abort_record.incarnation();
    let system_plane = SystemPlane::new(abort_record.clone(), id.clone(), target_system.clone());
    let actor_handle = ActorHandle::with_lane_flags(
        id.clone(),
        tx,
        stop_lane,
        mailbox_capacity,
        status_rx,
        priority_tx,
        priority_capacity.map(|_| Arc::new(LaneFlags::new(priority_by_alias))),
    );

    // The registration comes first, before the context and the subtree hold
    // exist, so a spawn the registry refuses creates no hold and leaves the
    // parent's stop budget as it was. The guard is built at once, so
    // anything that unwinds before the task exists unregisters the actor.
    // The registration happens BEFORE the task is spawned; the AbortHandle
    // does not exist yet at this point (it only exists once `handle.spawn`
    // returns a JoinHandle), so it is attached in a second step just below.
    //
    // `ActorContext::new` below runs with the ORIGINAL `system`
    // argument (possibly `None`), so a bare-spawned actor's `ctx.system`
    // stays `None`; this is harmless because every downstream spawn path
    // re-applies the same default-system fallback.
    // A refused spawn leaves no identity behind: the record that names it is
    // dropped with the spawn.
    let registration_seq = target_system
        .register_actor_as::<A>(
            &id,
            name.is_some(),
            &actor_handle,
            registry_parent(is_root, birth.parent),
            turn_clock.clone(),
            kill_grace,
            handle.clone(),
            incarnation,
        )
        .map_err(|err| refused(Some(&target_system), name.as_deref(), err))?;
    // A system that took up resolving identities between the read above and
    // the insert may have run its pass before the entry existed. The read
    // here follows the insert under the shard's lock, and the pass reads the
    // shard under the same lock after the engagement: either the pass saw
    // the entry, or this read sees the engagement and records the identity.
    if !bound && mode.word() & crate::telemetry::RESOLVING != 0 {
        let _ = target_system.resolve_identity(&abort_record, &id);
    }
    let guard = RegistryGuard::new(target_system.clone(), id.clone(), registration_seq)
        .with_status(status_tx.clone());

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
        turn_clock.clone(),
        subtree.clone(),
    );
    // The debug options of the spawn start the incarnation's debug state; an
    // actor spawned with none holds nothing.
    if let Some(debug) = crate::actor::sys_debug::DebugState::from_options(sys.debug) {
        context.set_debug(debug);
    }
    if let Some(sup) = context.supervision_ref() {
        sup.verdicts.set_supervisor(id.clone());
        // A supervisor traps exits from its creation.
        actor_handle.stop_lane_ref().abort_record().set_trap(true);
    }

    let guard = Some(guard.with_subtree(subtree.clone()));

    let marker = context.task_marker();
    let owner = Owner::new(id.clone(), sensitive);
    #[cfg(test)]
    let abort_reason = AbortReason::new(status_tx.clone(), marker.clone(), id.clone());
    #[cfg(not(test))]
    let abort_reason = AbortReason::new(status_tx.clone(), marker.clone());
    // Taken before the original moves into the run loop's parameters: a caller
    // that asked to keep a sender of the plane holds the one extra.
    let kept_status = keep_status.then(|| status_tx.clone());
    let run = MarkedRun::new(
        marker,
        abort_incarnation,
        run_actor(Box::new(RunParts {
            _subtree: subtree.clone(),
            status: status_tx,
            actor_slot: CaughtDrop::new(actor, |actor, _| end_value(actor), owner.clone()),
            ctx_slot: CaughtDrop::new(context, end_context, owner.clone()),
            mailbox_slot: QueueSlot::new(Some(rx), &target_system, owner.clone()),
            priority_slot: QueueSlot::new(priority_rx, &target_system, owner),
            _abort_reason: abort_reason,
            system_rx: system_plane,
            stop_rx,
            turn_clock: TurnGuard::new(turn_clock),
            registry_guard: guard,
            ack: AnswerHold::new(ack),
            workers,
            same_thread,
        })),
    );
    emit_actor_spawned(
        &target_system,
        actor_handle.stop_lane_ref().abort_record(),
        &id,
        name.as_deref(),
        mailbox_capacity,
        priority_capacity,
    );
    // A supervised restart's slot is told of the incarnation before its task
    // can run: a reader of the slot that finds nothing published then knows
    // that no incarnation of the attempt can have ended, and one that finds
    // the plane reads the end of an incarnation that ran.
    if let Some(publish) = birth.publish {
        publish.io.publish(
            actor_handle.stop_lane(),
            actor_handle.status_rx(),
            *publish.own,
            Some(subtree_watch.clone()),
        );
    }
    // Every poll of the task, and its drop, runs inside the incarnation's
    // span when a subscriber is interested in it, the subscriber's calls
    // inside a catch.
    let join = span.spawn(&handle, run);

    // Attach the abort handle to the just-created registry entries. Any
    // shutdown sweep that lands in the brief window before this call treats
    // the entry as not-yet-abortable (see `AnyActorHandle::abort`).
    target_system.attach_abort(&id, join.abort_handle(), registration_seq);

    Ok((actor_handle, join, kept_status, subtree_watch))
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
/// `ended` is the end of the task when the caller already took it from the
/// join handle (a restart attempt that handed over an incarnation which had
/// ended): the runner uses it instead of polling the spent handle again.
/// `grace` is the incarnation's kill grace: the time between its `Kill` and
/// its abort. The returned handle identifies the task in the plane. The
/// caller has made the runner's claim ([`claim_death`]) before this call.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_runner(
    death_set: &mut JoinSet<DeathOutcome>,
    child_id: ActorId,
    incarnation: u64,
    join: JoinHandle<StopReason>,
    ended: Option<StopReason>,
    lane: StopLane,
    io: SlotIo,
    guard: ChildLinkGuard,
    grace: Duration,
    owner: Option<IncarnationRef>,
) -> AbortHandle {
    death_set.spawn(async move {
        let mut guard = guard;
        let mut join = join;
        let mut io = io;
        let reason = run_ladder(&mut join, ended, &lane, &mut io, grace).await;
        // The ladder returned the incarnation's end: its reason is final, and
        // the record says whether the end was an ignored start.
        let ignored = lane.abort_record().ended_ignored();
        io.record_fate(ChildFate {
            reason: reason.clone(),
            incarnation,
            ignored,
        });
        // The child has terminated and its fate is recorded: the guard has
        // nothing left to force, so its drop below must not raise Kill,
        // rewrite the terminal status, or occupy the reaper.
        guard.disarm();
        if let Some(owner) = &owner {
            crate::telemetry::death_landed(owner, incarnation);
        }
        (child_id, RunnerEnd::new(reason, ignored), incarnation)
    })
}

/// Records, for the supervisor whose record is `owner`, that a runner
/// follows the child incarnation `incarnation`, whose record is `child`: the
/// supervisor owes a decision on that incarnation's death from the child's
/// end, which is recorded before the runner's join resolves, and a child
/// that has already ended is owed at once. Made before the attempt that
/// started the incarnation settles, so no reader finds the attempt settled
/// and its ended incarnation not yet owed.
pub(crate) fn claim_death(
    owner: Option<&IncarnationRef>,
    child_id: &ActorId,
    incarnation: u64,
    child: &IncarnationRef,
) {
    if let Some(owner) = owner {
        #[cfg(test)]
        end_hooks::run(end_hooks::CLAIM, child_id);
        #[cfg(not(test))]
        let _ = child_id;
        crate::telemetry::death_claim(owner, incarnation, child);
    }
}
