//! Actor task spawning and the message-dispatch run loop.
//!
//! The run loop's `select!` is `biased` with five tiers, highest priority
//! first: the stop lane, then the system channel (`GetStatus`), then the
//! supervision restart plane (restart attempts completing), then the death
//! plane (child watcher completions), then the user mailbox. This ordering is
//! deliberate, not incidental: it mirrors the OTP spirit of signals and
//! supervisor traffic pre-empting ordinary message-queue processing, so a
//! stop/kill request is honored and a child's death is observed even while
//! the mailbox is full, and `get_status` answers without waiting behind user
//! traffic. Every tier is only ever observed at a turn boundary - between two
//! `handle` invocations, never during one - so none of this races or cancels
//! an in-flight handler (the Isolated Turn Principle).
//!
//! A restart attempt runs entirely in the parent's own restart plane (a
//! `JoinSet<RestartOutcome>`): the run loop never blocks on one, and a
//! restart is initiated only from this loop - a `handle` invocation may
//! trigger one (a manual bounce, `restart_child`) but the loop itself always
//! owns the actual spawn. An attempt's outcome - adopted, or reported as an
//! ordinary failure - is the `JoinSet`'s own returned value; nothing about a
//! restart travels over any channel of any capacity.
//!
//! The stop lane itself is a `watch` signal, not a channel: raising it is
//! synchronous and infallible, severity only ever increases, and multiple
//! raises that land between two turn boundaries coalesce into the single
//! highest-severity, latest state - the loop is guaranteed at least one fresh
//! observation per distinct severity level reached, never one per individual
//! raise.
//!
//! A supervised child's death is delivered through its own per-incarnation
//! fate cell, populated by a watcher task living in the supervisor's death
//! plane (a `JoinSet`), never through a shared channel of any bounded
//! capacity - so however many children die in the same instant, none of
//! their fates are lost waiting for room. A task dying with
//! `StopReason::Kill` never awaits its own children at all: it drops its
//! supervision state directly, which aborts every live child's watcher task
//! in one step; each watcher's captured link guard still runs its `Drop` even
//! though it never got to observe its child's real exit, raising Kill on
//! that child's lane and repeating the same inversion one level down. A
//! child that does not exit on its own within its grace window is aborted by
//! the system's reaper task instead.
//!
//! Every actor, supervisor or not, owns a forwarder plane (also a `JoinSet`)
//! holding its timer and stream forwarder tasks. Draining it is folded into
//! the same combined branch as the supervision planes, biased last: a fired
//! one-shot, a finished stream, or a forwarder that panicked is reaped there,
//! which frees its registration and - for a panic - records it as the
//! actor's last forwarder error. None of this ever stops the actor; the
//! forwarder plane itself is torn down, along with everything still in it,
//! whenever the actor's context is dropped.

use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinError, JoinHandle, JoinSet};
use tokio::time::{Duration, Instant};

use crate::actor::panic::{catch_callback, payload_into_string};
use crate::actor::supervision::{
    advance_restart_chain, evaluate_strategy, ChildLifecycle, ChildLinkGuard, ChildRegistry,
    ChildState, DeathOutcome, GroupPhase, GroupRestart, RestartOutcome, StrategyOutcome,
    SupervisionConfig, SupervisionState, KILL_GRACE,
};
use crate::actor::{context::ActorContext, handle::ActorHandle, Actor, ActorEnvelope};
use crate::error::{ActorError, SpawnError, SupervisionError};
use crate::system::{ActorSystem, RegistryGuard};
use crate::types::{
    ActorId, ActorStatus, ActorStatusInfo, ChildEvent, ChildFate, ChildStoppedInternal, Envelope,
    LaneState, RestartStrategy, RestartType, Shutdown, StopLane, StopReason, SupervisionAction,
    SystemMessage,
};

/// Configuration for the actor's mailbox.
#[derive(Debug, Clone)]
pub struct MailboxConfig {
    /// The maximum number of messages the mailbox can hold.
    pub capacity: usize,
}

impl Default for MailboxConfig {
    fn default() -> Self {
        Self { capacity: 64 }
    }
}

impl MailboxConfig {
    /// Sets the mailbox capacity.
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
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
    /// Sets the mailbox capacity.
    pub fn with_mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox.capacity = capacity;
        self
    }

    /// Sets the complete mailbox configuration.
    pub fn with_mailbox(mut self, mailbox: MailboxConfig) -> Self {
        self.mailbox = mailbox;
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
) -> Result<ActorHandle<A>, SpawnError> {
    // Only reachable from the top-level `.detached()` spawn path: always a root.
    let (handle, _join, _status_tx) =
        spawn_actor(id.into(), actor, config.into(), name, system, None, true)?;
    Ok(handle)
}

/// Spawns an actor, returning the handle and the task's `JoinHandle`.
///
/// The task completes with the actor's final [`StopReason`]. For supervised
/// children the caller wraps the `JoinHandle` in a watcher
/// ([`spawn_watcher`]); the watcher, not the dying actor, is the authoritative
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
/// The third element of the returned tuple is the actor's own status sender:
/// a clone of the same one driving [`ActorHandle::status`], handed back so a
/// supervising caller can build a [`ChildLinkGuard`](crate::actor::supervision::ChildLinkGuard)
/// able to force-publish a terminal status for this specific incarnation.
type SpawnOutcome<A> = (
    ActorHandle<A>,
    JoinHandle<StopReason>,
    watch::Sender<ActorStatus>,
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
) -> Result<SpawnOutcome<A>, SpawnError> {
    let mailbox_capacity = config.mailbox.capacity;
    if mailbox_capacity == 0 {
        return Err(SpawnError::ZeroMailboxCapacity);
    }
    let handle = Handle::try_current().map_err(|_| SpawnError::MissingRuntime)?;

    // Every spawn always resolves to a concrete system (bare anonymous
    // spawns join `ActorSystem::default()` as roots, same as named spawns
    // already did) - resolved up front so the handle can carry the system's
    // stable identity, used by `ActorHandle`'s `Eq`/`Hash` alongside its
    // `ActorId`.
    let target_system = system.clone().unwrap_or_else(ActorSystem::default);

    let (tx, rx) = mpsc::channel(mailbox_capacity);
    let (system_tx, system_rx) = mpsc::channel::<SystemMessage>(64);
    let (status_tx, status_rx) = watch::channel(ActorStatus::Initializing);
    let (stop_lane, stop_rx) = StopLane::new();
    let actor_handle = ActorHandle::new(
        id.clone(),
        tx,
        system_tx,
        stop_lane,
        mailbox_capacity,
        status_rx,
        target_system.name().into(),
    );

    let supervision = config.supervision.map(SupervisionState::new);
    let context = ActorContext::new(
        id.clone(),
        actor_handle.clone(),
        handle.clone(),
        status_tx.clone(),
        system,
        name.clone(),
        supervision,
    );

    // The name-claim happens BEFORE the task is spawned; the AbortHandle
    // does not exist yet at this point (it only exists once `handle.spawn`
    // returns a JoinHandle), so it is attached in a second step just below.
    //
    // `ActorContext::new` above already ran with the ORIGINAL `system`
    // argument (possibly `None`), so a bare-spawned actor's `ctx.system`
    // stays `None`; this is harmless because every downstream spawn path
    // re-applies the same default-system fallback.
    let registration_seq =
        target_system.register_actor::<A>(&id, name.as_deref(), &actor_handle, is_root)?;
    let guard = Some(RegistryGuard::new(
        target_system.clone(),
        id.clone(),
        name.clone(),
        registration_seq,
    ));

    let join = handle.spawn(run_actor(
        actor, context, rx, system_rx, stop_rx, guard, ack,
    ));

    // Attach the abort handle to the just-created registry entries. Any
    // shutdown sweep that lands in the brief window before this call treats
    // the entry as not-yet-abortable (see `AnyActorHandle::abort`).
    target_system.attach_abort(&id, name.as_deref(), join.abort_handle());

    Ok((actor_handle, join, status_tx))
}

/// Spawns the watcher task for one child incarnation into the supervisor's
/// death plane.
///
/// The watcher owns the child's `JoinHandle<StopReason>`: it awaits the
/// task's completion (guaranteed by tokio to happen only after ALL the
/// child's drops, including its `RegistryGuard`, have finished - so the
/// registered name is already free once this resolves), derives the real
/// death reason (normal return, panic, or abort), and writes it into the
/// per-incarnation fate cell. It also carries `guard`, the child's link
/// guard, for the whole span of its own execution: if this task is ever
/// aborted before `join.await` resolves - most notably before it is ever
/// polled at all, when the supervisor's own death plane is dropped as part
/// of a Kill teardown - the guard's `Drop` still fires from the abandoned
/// future's own drop glue.
///
/// The task's own result - the child's id, its real stop reason, and its
/// incarnation - is picked up by the run loop via `JoinSet::join_next` on
/// the supervisor's death plane; no shared channel of any capacity sits
/// between a child's death and its supervisor observing it, so however many
/// children die at once, none of their fates are lost waiting for room.
pub(crate) fn spawn_watcher(
    death_set: &mut JoinSet<DeathOutcome>,
    child_id: ActorId,
    incarnation: u64,
    join: JoinHandle<StopReason>,
    fate_tx: watch::Sender<Option<ChildFate>>,
    guard: ChildLinkGuard,
) {
    death_set.spawn(async move {
        let mut guard = guard;
        let reason = match join.await {
            Ok(reason) => reason,
            Err(err) if err.is_panic() => match err.try_into_panic() {
                Ok(payload) => StopReason::Failure(ActorError::Panic(payload_into_string(payload))),
                Err(_) => {
                    StopReason::Failure(ActorError::Panic("panic payload unavailable".to_string()))
                }
            },
            Err(_) => StopReason::Cancelled,
        };
        fate_tx.send_replace(Some(ChildFate {
            reason: reason.clone(),
            incarnation,
        }));
        // The child has terminated and its fate is recorded: the guard has
        // nothing left to force, so its drop below must not raise Kill,
        // rewrite the terminal status, or occupy the reaper.
        guard.disarm();
        (child_id, reason, incarnation)
    });
}

// ---------------------------------------------------------------------------
// Actor run loop
// ---------------------------------------------------------------------------

/// Runs the actor's whole lifecycle: init, message loop, teardown.
///
/// `ack` (see [`spawn_actor`]) is resolved from exactly one of two places in
/// this function: the success arm right after the `Running` transition, or
/// the very end, after `registry_guard` has dropped, if init failed. That
/// ordering is load-bearing (N3 / OTP 26 corpse-consumed parity): it is why
/// the two former early-return blocks for `pre_start`/`on_started` failures
/// are now a labeled block that falls through to the shared teardown tail
/// instead of returning directly.
#[allow(clippy::too_many_arguments)]
async fn run_actor<A: Actor>(
    mut actor: A,
    mut ctx: ActorContext<A>,
    mut mailbox: mpsc::Receiver<ActorEnvelope<A>>,
    mut system_rx: mpsc::Receiver<SystemMessage>,
    mut stop_rx: watch::Receiver<LaneState>,
    registry_guard: Option<RegistryGuard>,
    mut ack: Option<oneshot::Sender<Result<(), ActorError>>>,
) -> StopReason {
    // Phase 1 + 2: pre_start/on_started validation (fail-fast gate). A panic
    // is a failed init, exactly like `Err` (self-cleaning contract:
    // on_stopped is not called). The outcome is captured instead of returned
    // directly so a failure still falls through to the shared teardown tail
    // below: the failure ack (if any) must only fire after the registry guard
    // has dropped, so an immediate same-name respawn can never race the dying
    // task and observe NameTaken (OTP 26 corpse-consumed parity).
    let init_result: Result<(), ActorError> = 'init: {
        // Phase 1: pre_start validation.
        let phase1 = match catch_callback(actor.pre_start(&mut ctx)).await {
            Ok(result) => result,
            Err(payload) => Err(ActorError::Panic(payload_into_string(payload))),
        };
        if let Err(err) = phase1 {
            break 'init Err(err);
        }

        // Phase 2: on_started initialization.
        let phase2 = match catch_callback(actor.on_started(&mut ctx)).await {
            Ok(result) => result,
            Err(payload) => Err(ActorError::Panic(payload_into_string(payload))),
        };
        if let Err(err) = phase2 {
            break 'init Err(err);
        }

        Ok(())
    };

    let mut stop_reason = match &init_result {
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
    };

    // Phase 3: Message loop - biased select! gives the stop lane top
    // priority, then the system channel, then user messages.
    // Never entered when init failed: the actor must never enter the loop.
    if init_result.is_ok() {
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
                                match catch_callback(actor.pre_stop(&reason, &mut ctx)).await {
                                    Ok(true) => {
                                        stop_reason = reason;
                                        break;
                                    }
                                    Ok(false) => continue, // Actor rejected the stop; a
                                        // later same-severity raise bumps the
                                        // generation and re-fires this gate.
                                    Err(payload) => {
                                        stop_reason = StopReason::Failure(ActorError::Panic(
                                            payload_into_string(payload),
                                        ));
                                        break;
                                    }
                                }
                            }
                            // Tier 2 (Failure/Cancelled): non-vetoable, on_stopped still runs
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
                            let info = build_status_info(&ctx);
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
                // tier. Pends forever with no supervision config or nothing
                // currently in flight on either plane, so this never becomes
                // a busy poll.
                event = next_supervision_event(&mut ctx) => {
                    match event {
                        Some(SupervisionEvent::Restart(Ok(outcome))) => {
                            match handle_restart_outcome(&mut actor, &mut ctx, outcome).await {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(reason) => {
                                    stop_reason = reason;
                                    break;
                                }
                            }
                        }
                        Some(SupervisionEvent::Restart(Err(_))) => {
                            // The restart task itself panicked or was aborted
                            // before returning its outcome. Any
                            // `ChildLinkGuard` it had already armed still ran
                            // its own drop glue as part of the abandoned
                            // future's teardown; there is nothing further to
                            // do here.
                        }
                        Some(SupervisionEvent::Death(Ok((child_id, child_reason, incarnation)))) => {
                            let event = ChildStoppedInternal { child_id, reason: child_reason, incarnation };
                            match handle_child_stopped(&mut actor, &mut ctx, event).await {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(reason) => {
                                    stop_reason = reason;
                                    break;
                                }
                            }
                        }
                        Some(SupervisionEvent::Death(Err(_))) => {
                            // The watcher task itself panicked or was aborted
                            // before completing (see `ChildLinkGuard`); its
                            // own drop glue already signalled and scheduled
                            // the child, so there is nothing further to do
                            // here.
                        }
                        Some(SupervisionEvent::Forwarder(outcome)) => {
                            // A fired one-shot, a finished stream, or a
                            // forwarder that panicked - reaping it here frees
                            // its registration and, on a panic, records it as
                            // the last forwarder error. Never stops the actor.
                            ctx.reap_forwarder(outcome);
                        }
                        None => {} // unreachable: `next_supervision_event` never resolves to `None`
                    }
                }

                // User mailbox
                envelope = mailbox.recv() => {
                    match envelope {
                        Some(env) => {
                            match dispatch(&mut actor, &mut ctx, env).await {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(reason) => {
                                    stop_reason = reason;
                                    break;
                                }
                            }
                        }
                        None => break, // mailbox closed (all handles dropped)
                    }
                }
            }
        }

        // Phase 4: on_stopped (terminate/2 parity: runs on failures and panics,
        // skipped only for Kill / brutal_kill).
        if !matches!(stop_reason, StopReason::Kill) {
            ctx.set_status(ActorStatus::Stopping);
            match catch_callback(actor.on_stopped(&stop_reason, &mut ctx)).await {
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
    } // init_result.is_ok(): Phase 3 (loop) + Phase 4 (on_stopped)

    // Close the system receiver AND the stop lane BEFORE stopping children:
    // late system senders (escalation timers, get_status callers) fail fast
    // instead of parking, and closing the lane at this exact point is what
    // makes `ActorHandle::stop`/`ActorSystem::stop`/`kill` observe
    // `SendError::Closed` from here on - the same closing point the system
    // channel used before the lane existed.
    drop(system_rx);
    drop(stop_rx);

    // Phase 5: Stop all children (reverse start order, honoring Shutdown) -
    // except when this task is itself dying with Kill: that reason never
    // awaits its own children. Its supervision state - the death plane
    // included - is dropped directly instead, which aborts every live
    // child's watcher task; each one's link guard fires from its own drop
    // glue, raising Kill on that child's lane and repeating the same
    // inversion one level down, without this task ever awaiting the subtree
    // it is discarding.
    if matches!(stop_reason, StopReason::Kill) {
        drop(ctx.take_supervision());
    } else {
        stop_all_children(&mut ctx).await;
    }

    // Held for its Drop impl. RegistryGuard::drop unregisters the actor -
    // the registered name is freed HERE, strictly before the terminal status
    // write below. `wait_stopped()`/`status()` only observe `Stopped` after
    // this drop has already run, so a caller that wakes on that terminal
    // status can immediately reuse the name (spawn a same-name actor)
    // without racing this task's own teardown. Tokio also guarantees this
    // task's JoinHandle completion is observable (by the watcher) only
    // after all drops have finished, so the name is free before the parent
    // can act on the death either way.
    drop(registry_guard);

    ctx.set_status(ActorStatus::Stopped);

    #[cfg(feature = "tracing")]
    tracing::info!(
        actor_id = %ctx.actor_id(),
        reason = %stop_reason,
        "Actor stopped"
    );

    // The failure ack (if any) fires only now, after the registry guard has
    // dropped: an immediate same-name respawn from the caller can never
    // observe NameTaken (OTP 26 corpse-consumed parity).
    if let Err(err) = init_result {
        if let Some(tx) = ack {
            let _ = tx.send(Err(err));
        }
    }

    stop_reason
}

/// Polls the supervisor's death plane for the next child watcher completion.
/// Pends forever when there is no supervision config, or the death plane
/// currently holds no live watcher, so this is safe to use as a `select!`
/// branch without ever turning into a busy poll.
/// One event from either of the supervisor's two `JoinSet` planes, as
/// returned by [`next_supervision_event`].
enum SupervisionEvent {
    /// A restart attempt resolved (see the restart plane, `restart_set`).
    Restart(Result<RestartOutcome, JoinError>),
    /// A child watcher completed (see the death plane, `death_set`).
    Death(Result<DeathOutcome, JoinError>),
    /// A timer or stream forwarder task completed (see the forwarder plane).
    /// Present on every actor, supervisor or not.
    Forwarder(Result<(tokio::task::Id, ()), JoinError>),
}

/// Polls the supervisor's restart and death planes together with every
/// actor's forwarder plane, biased in that order. All three live behind the
/// same `&mut ctx` borrow because `select!` constructs every branch's future
/// eagerly - disjoint fields reachable from one borrow can share it, but two
/// separate top-level `select!` branches each borrowing `ctx` themselves
/// cannot. Pends forever with no supervision config and nothing currently in
/// flight on any plane, so this is safe to use as a `select!` branch without
/// ever turning into a busy poll.
async fn next_supervision_event<A: Actor>(ctx: &mut ActorContext<A>) -> Option<SupervisionEvent> {
    let (planes, forwarders) = ctx.supervision_planes_mut();
    let forwarders_pending = !forwarders.is_empty();
    match planes {
        Some((restart_set, death_set)) => {
            let restart_pending = !restart_set.is_empty();
            let death_pending = !death_set.is_empty();
            if !restart_pending && !death_pending && !forwarders_pending {
                return std::future::pending::<Option<SupervisionEvent>>().await;
            }
            tokio::select! {
                biased;
                r = restart_set.join_next(), if restart_pending => {
                    r.map(SupervisionEvent::Restart)
                }
                d = death_set.join_next(), if death_pending => {
                    d.map(SupervisionEvent::Death)
                }
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
) -> ControlFlow<StopReason> {
    match envelope {
        Envelope::Message { payload, responder } => {
            match catch_callback(actor.handle(payload, ctx)).await {
                Ok(Ok(response)) => {
                    if let Some(tx) = responder {
                        let _ = tx.send(Ok(response));
                    }
                    ControlFlow::Continue(())
                }
                Ok(Err(err)) => {
                    // Cast-exception parity: an `Err` from `handle` stops
                    // the actor identically on the notify and send paths. The
                    // send path additionally has a caller to notify, so that
                    // delivery happens first when a responder is present.
                    if let Some(tx) = responder {
                        let _ = tx.send(Err(err.clone()));
                    }
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
                    ControlFlow::Break(StopReason::Failure(err))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Child supervision
// ---------------------------------------------------------------------------

/// Handles child-death events: manual-completion delivery, stale-incarnation
/// filtering, group-restart bookkeeping, strategy evaluation, restart
/// initiation, and the `on_child_stopped` callback.
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

    while let Some(ev) = work.pop_front() {
        // Manual-completion delivery: `stop_child`/`terminate_child` already
        // classified this exact incarnation's disposition the moment its real
        // fate was observed (see `ActorContext::manual_stop_child`) and
        // stored the resulting snapshot here, independent of whatever the
        // registry entry looks like by now - a same-handler `delete_child`
        // and respawn of the same name may already have replaced or removed
        // it. This is the ONLY place `on_child_stopped` fires for a manual
        // stop, and it fires from the snapshot, exactly once.
        let snapshot = ctx.supervision_mut().and_then(|sup| {
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
            // membership.
            let mut begin_chain: Option<Vec<ActorId>> = None;
            if let Some(sup) = ctx.supervision_mut() {
                if let Some(child) = sup.registry.get_mut(&ev.child_id) {
                    if let ChildLifecycle::Down {
                        incarnation,
                        event_pending,
                    } = child.lifecycle
                    {
                        if incarnation == ev.incarnation && event_pending {
                            let _ = child.transition(ChildLifecycle::Down {
                                incarnation,
                                event_pending: false,
                            });
                        }
                    }
                }
                if let Some(GroupPhase::Stopping(group)) = sup.pending_group.as_mut() {
                    if group.awaiting.remove(&ev.child_id) {
                        if let Some(child) = sup.registry.get_mut(&ev.child_id) {
                            // Guarded by incarnation, unlike the ordinary
                            // member_of_group arm below: a same-handler
                            // `delete_child` + same-name respawn (legal now
                            // that this member's override already settled
                            // it) reuses this exact `ActorId` for a brand
                            // new, unrelated, live incarnation - transitioning
                            // THAT one to `Down` would be a real bug, not the
                            // tolerated no-op this is everywhere else. A
                            // match means this is still the overridden
                            // incarnation itself (already `Down` from the
                            // synchronous commit; `Down` -> `Down` is a
                            // legal no-op).
                            if child.lifecycle.incarnation() == ev.incarnation {
                                let _ = child.transition(ChildLifecycle::Down {
                                    incarnation: ev.incarnation,
                                    event_pending: false,
                                });
                            }
                        }
                        if group.awaiting.is_empty() {
                            begin_chain = match sup.pending_group.take() {
                                Some(GroupPhase::Stopping(group)) => {
                                    let overrides = group.manual_overrides;
                                    Some(
                                        group
                                            .restart_order
                                            .into_iter()
                                            .filter(|id| !overrides.contains(id))
                                            .collect(),
                                    )
                                }
                                _ => None,
                            };
                        }
                    }
                }
            }
            if let Some(order) = begin_chain {
                start_restart_chain(ctx, order, &mut work);
            }
            if let Err(payload) = catch_callback(actor.on_child_stopped(&child_event, ctx)).await {
                return ControlFlow::Break(StopReason::Failure(ActorError::Panic(
                    payload_into_string(payload),
                )));
            }
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
            ctx.classify_manual_completion(&ev.child_id, ev.incarnation, kind, ev.reason.clone());
            work.push_front(ev);
            continue;
        }

        // Stale-incarnation filter: a superseded instance's death is not a
        // death of the child spec the registry currently tracks.
        let stale = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&ev.child_id))
            .map(|child| !child.accepts_incarnation(ev.incarnation))
            .unwrap_or(false);
        if stale {
            continue;
        }

        // Within a supervision tree an aborted task can only be our own Kill
        // escalation (AbortHandles are never exposed) - present it truthfully.
        let ev = {
            let known = ctx
                .supervision_ref()
                .map(|sup| sup.registry.get(&ev.child_id).is_some())
                .unwrap_or(false);
            if known && matches!(ev.reason, StopReason::Cancelled) {
                ChildStoppedInternal {
                    reason: StopReason::Kill,
                    ..ev
                }
            } else {
                ev
            }
        };

        // Group-membership path: deaths belonging to an in-flight
        // OneForAll/RestForOne restart (either phase).
        let mut member_of_group = false;
        let mut retry_front = false;
        let mut manual_override = false;
        let mut begin_chain: Option<Vec<ActorId>> = None;
        if let Some(sup) = ctx.supervision_mut() {
            match sup.pending_group.as_mut() {
                Some(GroupPhase::Stopping(group)) => {
                    if group.awaiting.remove(&ev.child_id) {
                        member_of_group = true;
                        manual_override = group.manual_overrides.contains(&ev.child_id);
                        if let Some(child) = sup.registry.get_mut(&ev.child_id) {
                            let incarnation = child.lifecycle.incarnation();
                            let _ = child.transition(ChildLifecycle::Down {
                                incarnation,
                                event_pending: false,
                            });
                        }
                        if group.awaiting.is_empty() {
                            begin_chain = match sup.pending_group.take() {
                                Some(GroupPhase::Stopping(group)) => {
                                    let overrides = group.manual_overrides;
                                    Some(
                                        group
                                            .restart_order
                                            .into_iter()
                                            .filter(|id| !overrides.contains(id))
                                            .collect(),
                                    )
                                }
                                _ => None,
                            };
                        }
                    } else {
                        // Independent failure while a group is pending: a
                        // failed restart attempt for a DIFFERENT member than
                        // the one being awaited must still settle that
                        // member's own ledger before anything else - the
                        // group's own machinery only ever revives a `Down`
                        // member (`Restarting` -> `Restarting` is not a
                        // legal transition), so leaving it `Restarting`
                        // forever wedges the chain permanently once it
                        // reaches this member's turn. A member the chain
                        // still means to restart is left for the chain
                        // itself to revive: the event is DROPPED instead of
                        // queued, since queueing it would double-evaluate a
                        // death the chain already supersedes (the group's
                        // single budget charge already happened at its
                        // triggering event). Anything else defers exactly as
                        // before, for ordinary post-group evaluation.
                        let superseded = settle_restarting_attempt(&mut sup.registry, &ev)
                            && group.restart_order.contains(&ev.child_id);
                        if !superseded {
                            sup.queued_triggers.push_back(ev);
                        }
                        continue;
                    }
                }
                Some(GroupPhase::Restarting(queue)) => {
                    if queue.front() == Some(&ev.child_id) {
                        // The in-flight member's restart failed (factory
                        // panic, spawn error, or the fresh incarnation died
                        // before adoption): retry it, charging the budget
                        // (OTP try_again_restart parity).
                        member_of_group = true;
                        retry_front = true;
                        if let Some(child) = sup.registry.get_mut(&ev.child_id) {
                            let incarnation = child.lifecycle.incarnation();
                            let _ = child.transition(ChildLifecycle::Down {
                                incarnation,
                                event_pending: false,
                            });
                        }
                    } else {
                        // Same hazard, same fix, for a member not currently
                        // at the front of the sequential chain (see the
                        // `GroupPhase::Stopping` arm above for the full
                        // rationale).
                        let superseded = settle_restarting_attempt(&mut sup.registry, &ev)
                            && queue.contains(&ev.child_id);
                        if !superseded {
                            sup.queued_triggers.push_back(ev);
                        }
                        continue;
                    }
                }
                None => {}
            }
        }

        if member_of_group {
            // A caller's `terminate_child` on this member during the group's
            // Stopping phase wins over the group's default disposition: it is
            // reported and left down instead of being folded into the
            // restart chain (`restart_order` was already filtered above).
            let mut action = SupervisionAction::RestartInitiated;
            if retry_front {
                let budget_ok = ctx
                    .supervision_mut()
                    .map(|sup| sup.budget.check_and_record())
                    .unwrap_or(false);
                if budget_ok {
                    initiate_restart(ctx, &ev.child_id);
                } else {
                    action = SupervisionAction::BudgetExhausted;
                }
            } else {
                // Starting the chain (the group's stop phase just completed)
                // is independent of whether THIS member's own disposition was
                // overridden: the chain excludes overridden members already
                // (`restart_order` was filtered when it was taken above), but
                // the group as a whole must still proceed for everyone else.
                if let Some(order) = begin_chain {
                    start_restart_chain(ctx, order, &mut work);
                }
                if manual_override {
                    prune_spec_if_needed(ctx, &ev.child_id);
                    action = SupervisionAction::Removed;
                }
            }
            let child_name = ctx
                .supervision_ref()
                .and_then(|sup| sup.registry.get(&ev.child_id))
                .and_then(|child| child.name.clone());
            let child_event = ChildEvent {
                child_id: ev.child_id.clone(),
                child_name,
                reason: ev.reason.clone(),
                action,
            };
            if let Err(payload) = catch_callback(actor.on_child_stopped(&child_event, ctx)).await {
                return ControlFlow::Break(StopReason::Failure(ActorError::Panic(
                    payload_into_string(payload),
                )));
            }
            if matches!(action, SupervisionAction::BudgetExhausted) {
                ctx.record_failure(SupervisionError::BudgetExhausted.into());
                return ControlFlow::Break(StopReason::ParentRequest);
            }
            continue;
        }

        // Normal path: mark dead, evaluate the strategy, act.
        let child_name = if let Some(sup) = ctx.supervision_mut() {
            if let Some(child) = sup.registry.get_mut(&ev.child_id) {
                let incarnation = child.lifecycle.incarnation();
                let _ = child.transition(ChildLifecycle::Down {
                    incarnation,
                    event_pending: false,
                });
                child.name.clone()
            } else {
                None
            }
        } else {
            None
        };

        let outcome = ctx
            .supervision_mut()
            .map(|sup| evaluate_strategy(sup, &ev.child_id, &ev.reason));

        let action = match outcome {
            None => SupervisionAction::NotSupervised,
            Some(StrategyOutcome::RestartOne(id)) => {
                initiate_restart(ctx, &id);
                SupervisionAction::RestartInitiated
            }
            Some(StrategyOutcome::RestartGroup {
                stop_reverse,
                restart_order,
            }) => {
                if stop_reverse.is_empty() {
                    // No live members to stop: go straight to the sequential
                    // restart chain.
                    start_restart_chain(ctx, restart_order, &mut work);
                } else {
                    begin_group_stop(ctx, &stop_reverse);
                    if let Some(sup) = ctx.supervision_mut() {
                        sup.pending_group = Some(GroupPhase::Stopping(GroupRestart {
                            awaiting: stop_reverse.iter().cloned().collect(),
                            restart_order,
                            manual_overrides: std::collections::HashSet::new(),
                        }));
                    }
                }
                SupervisionAction::RestartInitiated
            }
            Some(StrategyOutcome::Remove) => {
                prune_spec_if_needed(ctx, &ev.child_id);
                SupervisionAction::Removed
            }
            Some(StrategyOutcome::BudgetExhausted) => SupervisionAction::BudgetExhausted,
        };

        let child_event = ChildEvent {
            child_id: ev.child_id.clone(),
            child_name,
            reason: ev.reason.clone(),
            action,
        };
        if let Err(payload) = catch_callback(actor.on_child_stopped(&child_event, ctx)).await {
            return ControlFlow::Break(StopReason::Failure(ActorError::Panic(
                payload_into_string(payload),
            )));
        }

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

/// Settles a child's ledger from a stuck `Restarting` attempt back to `Down`
/// when `ev` reports that exact attempt's own failure - the event's
/// incarnation matches the ledger's not-yet-adopted `next` token. Returns
/// `true` when that was the case (and the transition was made), `false`
/// otherwise (the event belongs to some other incarnation, or the child
/// isn't `Restarting` at all).
///
/// This is the settlement half of the wedge fix for a group-pending death
/// event that belongs to neither the member currently being awaited (the
/// `Stopping` phase) nor the member at the front of the chain (the
/// `Restarting` phase): without it, a member whose own independent restart
/// attempt fails while a sibling's group teardown is still in flight is left
/// `Restarting` forever, because nothing else ever reports another outcome
/// for it, and the chain's own `advance_restart_chain`/`SupervisionState::initiate`
/// only ever revive a `Down` member.
fn settle_restarting_attempt(registry: &mut ChildRegistry, ev: &ChildStoppedInternal) -> bool {
    let matches_attempt = registry
        .get(&ev.child_id)
        .map(|child| {
            matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == ev.incarnation)
        })
        .unwrap_or(false);
    if matches_attempt {
        if let Some(child) = registry.get_mut(&ev.child_id) {
            let incarnation = child.lifecycle.incarnation();
            let _ = child.transition(ChildLifecycle::Down {
                incarnation,
                event_pending: false,
            });
        }
    }
    matches_attempt
}

/// Starts the sequential restart chain for a completed group: sets the
/// Restarting phase and hands the front of the queue to
/// [`advance_restart_chain`], which skips any member already fresher-`Running`
/// before this ever initiates a first attempt (sequential registration, OTP
/// left-to-right). An empty order (or one where every member turns out to
/// already be fresh) just drains the queued triggers.
fn start_restart_chain<A: Actor>(
    ctx: &mut ActorContext<A>,
    order: Vec<ActorId>,
    work: &mut VecDeque<ChildStoppedInternal>,
) {
    let queue: VecDeque<ActorId> = order.into();
    let next = if let Some(sup) = ctx.supervision_mut() {
        sup.pending_group = Some(GroupPhase::Restarting(queue));
        advance_restart_chain(sup, work)
    } else {
        None
    };
    if let Some(front) = next {
        initiate_restart(ctx, &front);
    }
}

/// Dispatches one restart attempt's outcome: an [`RestartOutcome::Adopted`]
/// value is handed to [`adopt_restart`]; a [`RestartOutcome::Failed`] one is
/// reported through the ordinary child-death path - budget-charged strategy
/// evaluation, exactly like any other failure - by synthesizing the same
/// [`ChildStoppedInternal`] event a watcher completion would have produced.
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
        } => {
            adopt_restart(
                actor,
                ctx,
                child_id,
                incarnation,
                new_stop_lane,
                new_join,
                guard,
            )
            .await
        }
        RestartOutcome::Failed {
            child_id,
            incarnation,
            reason,
        } => {
            let event = ChildStoppedInternal {
                child_id,
                reason,
                incarnation,
            };
            handle_child_stopped(actor, ctx, event).await
        }
    }
}

/// Adopts a restart attempt's [`RestartOutcome::Adopted`] value: validates
/// the incarnation token, builds the fresh instance's watcher and fate cell
/// (the guard itself was already armed inside the restart task, the instant
/// its `spawn_actor` call succeeded), advances a sequential group-restart
/// chain, and - when the chain completes - processes the failure events that
/// were queued while the group was in flight.
///
/// A member the group chain finds already `Running` on the very incarnation
/// being adopted here can only mean an independent restart (this same one)
/// completed while the group's own bookkeeping still expected to wait for
/// it - handled uniformly by [`advance_restart_chain`] on the NEXT member,
/// not by anything special here.
///
/// A member adopted while the group is still in its `Stopping` phase (an
/// independent restart that was already in flight when the group's failure
/// was evaluated, and so was never told to stop) is folded straight into
/// that same teardown instead of being left running alongside dying
/// siblings: it is issued the group's stop signal immediately and added to
/// the awaited set, preserving all-stopped-before-any-started.
#[allow(clippy::too_many_arguments)]
async fn adopt_restart<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    child_id: ActorId,
    seq: u64,
    new_stop_lane: StopLane,
    new_join: JoinHandle<StopReason>,
    guard: ChildLinkGuard,
) -> ControlFlow<StopReason> {
    let accepted = ctx
        .supervision_ref()
        .and_then(|sup| sup.registry.get(&child_id))
        .map(|child| matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == seq))
        .unwrap_or(false);
    if !accepted {
        // Superseded restart (a newer seq is pending or the spec is gone):
        // dropping the guard kills the stray instance through the ordinary
        // Kill-raise/status-force/reaper ladder instead of leaking a
        // duplicate. Its death event would carry a stale incarnation and be
        // ignored anyway.
        drop(guard);
        drop(new_join);
        return ControlFlow::Continue(());
    }

    // The abort handle is a plain getter (`&self`), so it can be taken here
    // without disturbing `new_join`, which the watcher below still needs by
    // value.
    let abort = new_join.abort_handle();
    let (fate_tx, fate_rx) = watch::channel(None);

    let mut next_in_chain: Option<ActorId> = None;
    let mut drained: VecDeque<ChildStoppedInternal> = VecDeque::new();
    let mut stop_signal: Option<(StopLane, Shutdown, AbortHandle)> = None;
    if let Some(sup) = ctx.supervision_mut() {
        spawn_watcher(
            &mut sup.death_set,
            child_id.clone(),
            seq,
            new_join,
            fate_tx,
            guard,
        );
        sup.registry.update_restarted(
            &child_id,
            seq,
            new_stop_lane.clone(),
            fate_rx,
            abort.clone(),
        );

        match sup.pending_group.as_mut() {
            Some(GroupPhase::Stopping(group)) if group.restart_order.contains(&child_id) => {
                group.awaiting.insert(child_id.clone());
                let shutdown = sup
                    .registry
                    .get(&child_id)
                    .map(|c| c.spec.shutdown)
                    .unwrap_or_default();
                stop_signal = Some((new_stop_lane, shutdown, abort));
            }
            Some(GroupPhase::Restarting(queue)) if queue.front() == Some(&child_id) => {
                queue.pop_front();
                next_in_chain = advance_restart_chain(sup, &mut drained);
            }
            _ => {}
        }
    }

    if let Some((lane, shutdown, abort)) = stop_signal {
        stop_group_member(lane, shutdown, abort);
    }
    if let Some(next) = next_in_chain {
        initiate_restart(ctx, &next);
    }
    for ev in drained {
        if let ControlFlow::Break(reason) = handle_child_stopped(actor, ctx, ev).await {
            return ControlFlow::Break(reason);
        }
    }
    ControlFlow::Continue(())
}

/// Sends stop signals to the live members of a group restart, in reverse
/// start order, honoring each child's `Shutdown` policy. `Timeout` arms a
/// fire-and-forget escalation timer that raises Kill at expiry (a no-op if
/// the child already stopped: the lane is closed by then).
fn begin_group_stop<A: Actor>(ctx: &mut ActorContext<A>, stop_reverse: &[ActorId]) {
    let mut targets: Vec<(StopLane, Shutdown, AbortHandle)> = Vec::new();
    if let Some(sup) = ctx.supervision_ref() {
        for id in stop_reverse {
            if let Some(child) = sup.registry.get(id) {
                targets.push((
                    child.stop_lane.clone(),
                    child.spec.shutdown,
                    child.abort.clone(),
                ));
            }
        }
    }
    for (lane, shutdown, abort) in targets {
        stop_group_member(lane, shutdown, abort);
    }
}

/// Sends one group member its stop signal, honoring its `Shutdown` policy.
/// `Timeout` arms a fire-and-forget escalation timer that raises Kill at
/// expiry (a no-op if the child already stopped: the raise is infallible
/// either way). Shared by [`begin_group_stop`] (the ordinary case: a member
/// already running when the group's failure was evaluated) and
/// [`adopt_restart`] (a member whose independent restart is adopted while the
/// group is still in its `Stopping` phase - folded into the same teardown
/// instead of being left running alongside its dying siblings).
fn stop_group_member(lane: StopLane, shutdown: Shutdown, abort: AbortHandle) {
    match shutdown {
        Shutdown::Kill => {
            lane.raise(StopReason::Kill);
            spawn_grace_abort(abort);
        }
        Shutdown::Timeout(after) => {
            lane.raise(StopReason::ParentRequest);
            let escalate = lane.clone();
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                // No-op if the child already stopped (lane closed / task
                // finished): raise is infallible either way.
                escalate.raise(StopReason::Kill);
                tokio::time::sleep(KILL_GRACE).await;
                abort.abort();
            });
        }
        Shutdown::Infinity => {
            lane.raise(StopReason::ParentRequest);
        }
    }
}

/// Arms the abort() backstop behind an already-sent Kill: after the grace, a
/// task that has not terminated cooperatively is aborted. Harmless if the
/// task already finished.
fn spawn_grace_abort(abort: AbortHandle) {
    tokio::spawn(async move {
        tokio::time::sleep(KILL_GRACE).await;
        abort.abort();
    });
}

/// Prunes a child's spec after a non-restart outcome when OTP says so:
/// Temporary specs are deleted as soon as the process terminates, and
/// SimpleOneForOne removes dynamic specs likewise. A transient child stopped
/// cleanly keeps its spec (`Down`, the pid=undefined analog) for
/// restart_child.
pub(crate) fn prune_spec_if_needed<A: Actor>(ctx: &mut ActorContext<A>, child_id: &ActorId) {
    if let Some(sup) = ctx.supervision_mut() {
        let prune = matches!(sup.config.strategy, RestartStrategy::SimpleOneForOne)
            || sup
                .registry
                .get(child_id)
                .map(|c| matches!(c.spec.restart_type, RestartType::Temporary))
                .unwrap_or(false);
        if prune {
            sup.registry.remove(child_id);
            sup.restart_fns.remove(child_id);
        }
    }
}

/// Initiates a non-blocking restart for a child using its stored restart
/// closure (which captured the child's id, name, and config by value).
fn initiate_restart<A: Actor>(ctx: &mut ActorContext<A>, child_id: &ActorId) {
    let seq = ctx.resolved_system().next_incarnation();
    if let Some(sup) = ctx.supervision_mut() {
        sup.initiate(child_id, seq);
    }
}

/// Stops all children in reverse start order, respecting their Shutdown
/// policies. Awaits each child's per-incarnation fate cell: a populated fate
/// implies the child task has fully terminated (its watcher only writes it
/// after the child's own `JoinHandle` resolves, which tokio guarantees only
/// after every one of the child's own drops have already run). `Timeout`
/// escalates to Kill at expiry. Never reached when this task is itself
/// dying with Kill - that path drops the supervision state directly instead
/// (see the `run_actor` cascade-inversion comment).
async fn stop_all_children<A: Actor>(ctx: &mut ActorContext<A>) {
    let children = match ctx.supervision_mut() {
        Some(sup) => {
            sup.restart_fns.clear();
            sup.pending_group = None;
            sup.queued_triggers.clear();
            sup.registry.drain_all()
        }
        None => return,
    };

    // Stop in reverse order (last started = first stopped)
    for child in children.into_iter().rev() {
        if !matches!(child.lifecycle, ChildLifecycle::Running(_)) {
            continue;
        }

        let ChildState {
            stop_lane: lane,
            abort,
            mut fate_rx,
            spec,
            ..
        } = child;
        match spec.shutdown {
            Shutdown::Kill => {
                lane.raise(StopReason::Kill);
                if await_fate(&mut fate_rx, KILL_GRACE).await.is_none() {
                    abort.abort();
                    // Final bound: only a non-yielding loop survives abort;
                    // detach rather than hang the parent's shutdown.
                    let _ = await_fate(&mut fate_rx, KILL_GRACE).await;
                }
            }
            Shutdown::Timeout(after) => {
                lane.raise(StopReason::ParentRequest);
                if await_fate(&mut fate_rx, after).await.is_none() {
                    // Escalate: a vetoing or slow child is brutally killed
                    // (OTP: exit(Child, kill) after the shutdown timeout),
                    // with the abort() backstop behind it.
                    lane.raise(StopReason::Kill);
                    if await_fate(&mut fate_rx, KILL_GRACE).await.is_none() {
                        abort.abort();
                        let _ = await_fate(&mut fate_rx, KILL_GRACE).await;
                    }
                }
            }
            Shutdown::Infinity => {
                lane.raise(StopReason::ParentRequest);
                let _ = fate_rx.wait_for(|fate| fate.is_some()).await;
            }
        }
    }
}

/// Awaits `fate_rx` reaching a populated fate, bounded by `bound`. Returns
/// the fate once observed, or `None` on timeout (the sender dropping without
/// ever populating the cell counts as a timeout here too - a stray case that
/// cannot happen on this path, since every watcher always writes its fate
/// before returning).
async fn await_fate(
    fate_rx: &mut watch::Receiver<Option<ChildFate>>,
    bound: Duration,
) -> Option<ChildFate> {
    match tokio::time::timeout(bound, fate_rx.wait_for(|fate| fate.is_some())).await {
        Ok(Ok(fate)) => fate.clone(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

/// Adds `dur` to `base`, saturating at the furthest representable instant
/// instead of panicking when the sum would overflow. A saturated deadline
/// is simply far enough in the future that it never fires within any
/// realistic wait; the overflow case only exists to protect against a
/// pathological caller-supplied `Duration` (for example `Duration::MAX`).
///
/// Used everywhere a deadline is computed from `Instant::now()` plus a
/// caller-controlled duration: `send_timeout`, system shutdown deadlines,
/// and timer scheduling.
pub(crate) fn saturating_deadline(base: Instant, dur: Duration) -> Instant {
    if let Some(deadline) = base.checked_add(dur) {
        return deadline;
    }
    // `dur` overflows `base`: binary-search the largest addable duration so
    // the result still saturates instead of panicking, regardless of the
    // platform's internal `Instant` representation.
    let mut lo = Duration::ZERO;
    let mut hi = dur;
    while hi - lo > Duration::from_secs(1) {
        let mid = lo + (hi - lo) / 2;
        if base.checked_add(mid).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    base.checked_add(lo).unwrap_or(base)
}

fn build_status_info<A: Actor>(ctx: &ActorContext<A>) -> ActorStatusInfo {
    let (child_count, name) = ctx
        .supervision_ref()
        .map(|sup| (sup.registry.len(), ctx.actor_name().cloned()))
        .unwrap_or((0, ctx.actor_name().cloned()));

    ActorStatusInfo {
        id: ctx.actor_id().clone(),
        name,
        status: ctx.status(),
        mailbox_len: ctx.self_handle().mailbox_len(),
        mailbox_capacity: ctx.self_handle().mailbox_capacity(),
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
}
