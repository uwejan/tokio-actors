//! Actor task spawning and the message-dispatch run loop.
//!
//! The run loop's `select!` is `biased`, highest priority first: the stop lane,
//! then the system channel (status reads, the drain of the signal inbox for
//! links and exits, `SupervisorRef` requests and `sys` calls), then the
//! supervision restart plane
//! (restart attempts completing), then the death plane (child runner
//! completions), then the verdict signal (an incarnation given the Unresponsive
//! verdict), then the forwarder plane (timer and stream completions), then the
//! priority lane (present only on an actor spawned with one), then the user
//! mailbox, and last the idle spell's threshold, for an actor that set one.
//! This ordering is deliberate, not incidental: a stop or kill
//! request is observed, a child's death is observed, and (on an actor spawned
//! with a priority lane) a priority message is received even while the mailbox
//! is full, and `get_status` answers without waiting behind user traffic.
//! Every branch is only ever observed at a turn boundary, between two
//! callbacks and never during one, so none of this races or cancels a callback
//! already in progress. A callback that is still awaiting is cancelled only by
//! the abort that backs a `Kill`, by the abort of a start that outruns its
//! `start_timeout` (or whose timeout bound panics before it decides), and by
//! the end of the actor's runtime. The abort that backs a `Kill` is issued
//! once the kill grace has passed since the `Kill` was raised; for a
//! `stop_within` call whose reason is `Kill`, once its window has passed in
//! the grace's place; when more than one call raised a `Kill` on the same
//! actor, once the earliest of their deadlines has passed; and at once on a
//! runtime built without the time driver (with `panic = "abort"` the first timer the system uses on such a
//! runtime ends the process instead).
//!
//! The priority lane is served ahead of the mailbox at every turn, so a
//! priority flood at or above the rate the actor consumes it starves the normal
//! mailbox for as long as it lasts (the lane's capacity bounds its memory and
//! parks a flooding sender, and no message is lost). A sustained storm of
//! supervision events, which sit above the lane, delays the lane and the
//! mailbox alike. Reception order, highest first: stop lane, system channel,
//! restart plane, death plane, verdict signal, forwarder plane, priority lane,
//! mailbox; an actor that set a receive timeout or an idle length reads that
//! threshold after them all; a stop request never travels on the priority
//! lane.
//!
//! A restart attempt runs in the parent's restart plane (a
//! `JoinSet<RestartOutcome>`), initiated by the run loop or by a handler (a
//! bounce, `restart_child`): the run loop never blocks on one. An attempt first
//! waits, still in the restart plane, until the previous incarnation's whole
//! subtree has ended (every actor it spawned, and every actor those spawned),
//! bounded by the longest chain of stop budgets in the subtree (a worker's kill
//! grace, a supervisor child's `Shutdown` window plus its kill grace, and for a
//! worker that has called `trap_exit` its own `Shutdown` window as well), each
//! followed by a short settle time, plus one more settle time, and unbounded
//! only while a supervisor child under `Shutdown::Infinity` runs in it, or a
//! worker under `Infinity` that traps exit signals, or a budget is too large to
//! represent. Past that bound it gives no verdict on an actor before the abort
//! the system owes it has had the settle time to end it, waiting for an abort
//! that is late for at most the bound once more plus one more settle time. It
//! honors a stop order during that wait. The attempt then owns its incarnation
//! from the factory to the init ack, honoring a stop order at every stage, and
//! hands the incarnation over with its outcome; the loop then spawns the
//! incarnation's runner in the death plane, which owns it to its fate. An
//! attempt's outcome - adopted, cancelled before its factory ran, stopped under
//! an order, reported as an ordinary failure, or reported unresponsive because
//! its wait for the previous subtree outlived its bound - is the `JoinSet`'s
//! own returned value; nothing about a restart travels over any channel of any
//! capacity. An attempt whose wait expired is not evaluated: it is recorded as
//! down, and the supervisor stops with an unresponsive-child failure, so its
//! own supervisor decides.
//!
//! The stop lane itself is a cell in state that the actor's task and its
//! handles share, not a channel. Raising it is synchronous and never waits for
//! capacity. Once the actor has run its `on_stopped` or gone past it (it does
//! not run under a `Kill` or after a start that failed), a raise other than
//! `Kill` is refused; a `Kill` is accepted until the actor seals the lane at
//! the end of its teardown, and every raise is refused after that, as it is
//! once the actor's task is gone. Severity never decreases. Raises that
//! land between two turn boundaries coalesce into one observation of the
//! strongest request (of two at the same severity, the one of higher rank,
//! otherwise the later one), so the loop observes that one request and not one
//! per raise, and a request that a stronger one overtakes before the loop reads
//! is never observed. A raise at the same severity after the loop has observed
//! the first is observed again.
//!
//! A supervised child's death is delivered through its own per-incarnation fate
//! cell, populated by that incarnation's runner task living in the supervisor's
//! death plane (a `JoinSet`), never through a shared channel of any bounded
//! capacity - so however many children die in the same instant, none of their
//! fates are lost waiting for room. The runner, the death-plane task that
//! awaits the incarnation's exit, also owns that incarnation's stop ladder: any
//! stop the supervisor orders for the incarnation, from any origin, is
//! escalated by the runner, so no caller future and no detached timer owns any
//! step of it; a bounded stop called on the child's handle is escalated by a
//! task the system owns. A
//! task dying with `StopReason::Kill` never awaits its own children: it drops
//! its supervision state, and each live child's link guard acts for itself. A
//! worker that does not trap exit signals has Kill raised on its lane and is
//! aborted by the system's reaper once its kill grace has passed. A child that
//! is itself a supervisor, or a worker that traps exit signals, is given
//! `StopReason::ParentKilled`; it runs `on_stopped` and stops its own children
//! in order within its own `Shutdown` window, the reaper raises Kill at the end
//! of that window and aborts the task once the child's kill grace has passed,
//! and such a child under `Shutdown::Infinity` has no window and is not
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

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinError, JoinHandle, JoinSet};
use tokio::time::{Duration, Instant};

use crate::actor::mailbox::{MailRx, QueueRx};
use crate::actor::panic::{
    catch_callback, catch_hook_t, catch_sync, contain_waker_panic, discard, payload_into_string,
    send_contained, AnswerHold,
};
use crate::actor::subtree::{AbortRecord, SubtreeHold, SubtreeLink, SubtreeWatch, SystemPlane};
use crate::actor::supervision::rules::{
    init_kill_check, strict_closed_ordered, strict_step, InitStep, StrictAckKind, StrictStep,
    TeardownAction,
};
use crate::actor::supervision::{
    advance_restart_chain, await_cell, evaluate_strategy_charged, rules, run_ladder, BudgetCharge,
    CellOutcome, CellWatch, ChainStep, ChildLifecycle, ChildLinkGuard, ChildState, DeathOutcome,
    DownPending, GroupMembership, GroupPhase, GroupRestart, GroupScope, Origin, ReportClass,
    RestartFn, RestartOutcome, Route, SlotIo, StrategyOutcome, SupervisionConfig, SupervisionState,
    TaskPlane, TaskState, DEFAULT_KILL_GRACE,
};
use crate::actor::sys::{DebugEntry, SysOptions};
use crate::actor::sys_loop::is_suspended;
use crate::actor::task_key::PollScope;
use crate::actor::turn::IdleKind;
use crate::actor::{
    context::{ActorContext, ForwarderEnd},
    handle::{ActorHandle, LaneFlags},
    link::InboxItem,
    Actor, ActorEnvelope,
};
use crate::actor::{sys_serve, sys_state};
use crate::error::{ActorError, SpawnError, SupervisionError};
use crate::helper::{EndGuard, HelperKind, HelperOwner};
use crate::system::{ActorSystem, RegistryGuard};
use crate::telemetry::reports;
use crate::telemetry::{
    armed_for, push_about, ActorRef, ActorSpan, AttemptOutcome, Birth, EventKind, HookKind,
    HotCache, Identity, SpawnTraced, TelemetryEvent, TurnOutcome,
};
use crate::types::turn_clock::{
    now_nanos, outcome_of, outcome_plain, push_run, push_turn, TurnGuard, TurnMode,
};
use crate::types::{
    ActorId, ActorStatus, ActorStatusInfo, ChildEvent, ChildFate, ChildStoppedInternal, DownReason,
    Envelope, Incarnation, LaneRx, LinkExit, MonitorId, RestartStrategy, RestartType, Shutdown,
    StatusTx, StatusValue, StopLane, StopReason, StopSeverity, SupervisionAction, SystemMessage,
    TurnClock,
};

/// Configuration for the actor's mailbox.
#[derive(Debug, Clone)]
pub struct MailboxConfig {
    /// The maximum number of messages the mailbox can hold. A value above
    /// [`tokio::sync::Semaphore::MAX_PERMITS`] is clamped to it at spawn
    /// time instead of being rejected. The default is 64. A larger value is a
    /// per-actor opt-in: a capacity of `usize::MAX` never makes a sender wait,
    /// so a producer that outruns the actor grows the queue until memory runs
    /// out.
    pub capacity: usize,
    /// The priority lane's bounded capacity. `None` (the default) means the
    /// actor has no priority lane at all, in which case every `*_priority`
    /// method on its handle behaves exactly like its normal-lane sibling.
    /// There is no default capacity for an enabled lane: see
    /// [`with_priority_lane`](Self::with_priority_lane).
    pub priority_capacity: Option<usize>,
    /// Leaves the priority lane to priority aliases: the four `*_priority`
    /// methods of a handle then use the ordinary mailbox, exactly as on an
    /// actor with no lane, and a sender reaches the lane only through an alias
    /// made with [`AliasOptions::priority`](crate::AliasOptions::priority).
    /// The actor's own `monitor_priority` and `link_priority` notices and
    /// `exit_priority` still take the lane. The default is `false`: any holder
    /// of the handle can send on the lane. See [`with_priority_by_alias`](Self::with_priority_by_alias).
    pub priority_by_alias: bool,
}

impl Default for MailboxConfig {
    fn default() -> Self {
        Self {
            capacity: 64,
            priority_capacity: None,
            priority_by_alias: false,
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
    /// the restart plane, the death plane, the verdict signal, the forwarder
    /// plane, this lane, and the normal mailbox; a stop request never
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
    /// A plain Erlang process has one message queue, whose length is not otherwise observable and which never applies
    /// backpressure. This lane diverges on both counts: its length and capacity
    /// are observable here
    /// ([`ActorHandle::priority_len`](crate::actor::handle::ActorHandle::priority_len),
    /// [`ActorHandle::priority_capacity`](crate::actor::handle::ActorHandle::priority_capacity)),
    /// and, being bounded, a full lane applies backpressure (`Full`, a timeout)
    /// instead of growing without limit. It also differs in who may use it: any
    /// holder of this actor's handle can send on the lane unless the actor
    /// leaves it to its priority aliases
    /// ([`with_priority_by_alias`](Self::with_priority_by_alias)), where
    /// Erlang/OTP lets a sender use priority only through an alias the receiver
    /// created for it, so the receiver chooses who may.
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

    /// Leaves the priority lane to priority aliases, so the receiver chooses
    /// who may use it.
    ///
    /// With this set, the four `*_priority` methods of
    /// [`ActorHandle`] send on the ordinary mailbox, exactly as they do on an
    /// actor with no lane, and an alias made with
    /// [`AliasOptions::priority`](crate::AliasOptions::priority) is the way for
    /// a sender to reach the lane. An alias's priority sends and
    /// [`exit_priority`](crate::actor::context::ActorContext::exit_priority)
    /// ride the lane whether this is set or not, and so do the actor's own
    /// `monitor_priority` and `link_priority` notices. Without a priority lane
    /// ([`with_priority_lane`](Self::with_priority_lane)) this changes
    /// nothing.
    ///
    /// Erlang/OTP: a sender uses priority only through an alias the receiver
    /// created for it (`alias/1` with the `priority` option).
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::MailboxConfig;
    ///
    /// let config = MailboxConfig::default()
    ///     .with_priority_lane(8)
    ///     .with_priority_by_alias();
    /// assert!(config.priority_by_alias);
    /// ```
    pub fn with_priority_by_alias(mut self) -> Self {
        self.priority_by_alias = true;
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
    /// time instead of being rejected. The default is 64. A larger value is a
    /// per-actor opt-in: a capacity of `usize::MAX` never makes a sender wait,
    /// so a producer that outruns the actor grows the queue until memory runs
    /// out.
    pub fn with_mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox.capacity = capacity;
        self
    }

    /// Sets the complete mailbox configuration.
    pub fn with_mailbox(mut self, mailbox: MailboxConfig) -> Self {
        self.mailbox = mailbox;
        self
    }

    /// Leaves the priority lane to priority aliases (shorthand for
    /// [`MailboxConfig::with_priority_by_alias`] through
    /// [`with_mailbox`](Self::with_mailbox)): the four `*_priority` methods
    /// of a handle then use the ordinary mailbox, and a sender reaches the lane
    /// only through a priority alias.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::ActorConfig;
    ///
    /// let config = ActorConfig::default()
    ///     .with_priority_lane(8)
    ///     .with_priority_by_alias();
    /// assert!(config.mailbox.priority_by_alias);
    /// ```
    pub fn with_priority_by_alias(mut self) -> Self {
        self.mailbox.priority_by_alias = true;
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
    /// the restart plane, the death plane, the verdict signal, the forwarder
    /// plane, this lane, and the normal mailbox; a stop request never
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
    /// A plain Erlang process has one message queue, whose length is not otherwise observable and which never applies
    /// backpressure. This lane diverges on both counts: its length and capacity
    /// are observable here
    /// ([`ActorHandle::priority_len`](crate::actor::handle::ActorHandle::priority_len),
    /// [`ActorHandle::priority_capacity`](crate::actor::handle::ActorHandle::priority_capacity)),
    /// and, being bounded, a full lane applies backpressure (`Full`, a timeout)
    /// instead of growing without limit. It also differs in who may use it: any
    /// holder of this actor's handle can send on the lane unless the actor
    /// leaves it to its priority aliases
    /// ([`with_priority_by_alias`](Self::with_priority_by_alias)), where
    /// Erlang/OTP lets a sender use priority only through an alias the receiver
    /// created for it, so the receiver chooses who may.
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
    record: &AbortRecord,
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
    record: &AbortRecord,
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
    let sys_bits = (u8::from(sys.allow_state_access) * crate::actor::subtree::SYS_ALLOW_STATE)
        | (u8::from(sensitive) * crate::actor::subtree::SYS_SENSITIVE);
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
    // The name-claim happens BEFORE the task is spawned; the AbortHandle
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
            name.as_ref(),
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
    let guard = RegistryGuard::new(
        target_system.clone(),
        id.clone(),
        name.clone(),
        registration_seq,
    )
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
    target_system.attach_abort(&id, name.as_deref(), join.abort_handle(), registration_seq);

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
    owner: Option<AbortRecord>,
) -> AbortHandle {
    death_set.spawn(async move {
        let mut guard = guard;
        let mut join = join;
        let mut io = io;
        let reason = run_ladder(&mut join, ended, &lane, &mut io, grace).await;
        io.record_fate(ChildFate {
            reason: reason.clone(),
            incarnation,
        });
        // The child has terminated and its fate is recorded: the guard has
        // nothing left to force, so its drop below must not raise Kill,
        // rewrite the terminal status, or occupy the reaper.
        guard.disarm();
        if let Some(owner) = &owner {
            crate::telemetry::death_landed(owner, incarnation);
        }
        (child_id, reason, incarnation)
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
    owner: Option<&AbortRecord>,
    child_id: &ActorId,
    incarnation: u64,
    child: &AbortRecord,
) {
    if let Some(owner) = owner {
        #[cfg(test)]
        end_hooks::run(end_hooks::CLAIM, child_id);
        #[cfg(not(test))]
        let _ = child_id;
        crate::telemetry::death_claim(owner, incarnation, child);
    }
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
/// status plane closes, then its subtree ends. The answer to a request whose
/// handler failed goes out in between, once the name is free and the stop lane
/// is sealed and before the queued messages drop, unless a `Kill` ended the
/// actor (see `HeldAnswer`). The name is free before the
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
///   the forwarders, `stop_rx` after sealing the lane, the held answer of a
///   failed request (sent, or dropped under a `Kill`), `ack` (answered for a
///   failed init), `mailbox_slot` and `priority_slot` (the queued messages,
///   one at a time), `actor_slot` and `ctx_slot`, publishes `Stopped`
///   through `status`, and the return drops `turn_clock`, `status` and
///   `_subtree`, last to first. No await follows the seal.
/// - Aborted while suspended, or unwound by this task's own panic: the
///   body's locals (the held answer's sender among them, so its caller waits
///   for the reason, which is written once the name is free), then the
///   parameters last to first, so `ack`,
///   `registry_guard`, `turn_clock`, `stop_rx`, `system_rx`, `_abort_reason`
///   (the reason is written: the name is free, and no queued message has
///   dropped), `priority_slot`, `mailbox_slot`, `ctx_slot`, `actor_slot`,
///   `status`, `_subtree`; whichever of them the tail already released is
///   skipped. The sender of the start ack, the held answer and the sender of
///   the request in hand each sit in an [`AnswerHold`], which releases it
///   inside a catch: waking the caller of a spawn or of a request, whose waker
///   may panic, adds no second panic to an unwind.
/// - Aborted before the first poll: first to last, so `_subtree`, `status`,
///   `actor_slot`, `ctx_slot` (the context writes the reason), `mailbox_slot`,
///   `priority_slot`, `_abort_reason`, `system_rx`, `stop_rx`, `turn_clock`,
///   `registry_guard`, `ack`. The ack is answered last there, after the
///   queued messages drop: an awaited spawn with a start timeout is bounded
///   by that timeout and the settle wait after its abort. When no handle
///   outside the actor is left, the last reference to the incarnation's abort
///   block drops with `stop_rx` instead of `system_rx`, still before
///   `turn_clock`, `registry_guard` and `ack`.
///
/// The comments on `_subtree`, `status` and `registry_guard` say how each
/// path keeps the order.
/// The run loop's inputs, boxed so the run-loop future stores each of them
/// once: an `async fn` keeps every parameter twice (the unresumed state's copy
/// and the body's moved-in copy), and this box is the only parameter. The
/// fields keep the parameter order, so every documented drop order is the
/// same: before the first poll the box drops whole, its fields first to last;
/// from the first poll on they are the body's bindings, dropped last to first.
pub(crate) struct RunParts<A: Actor> {
    // The actor's hold on its subtree token, declared first. On return and
    // on an abort while suspended it is the last parameter dropped, so the
    // subtree outlives the actor value, the context and the queued
    // messages. On an abort before the first poll it is the first parameter
    // dropped, while the context and the registry guard still hold the
    // subtree: the guard, declared after everything the actor owns,
    // releases the last hold after it freed the name.
    _subtree: Arc<SubtreeHold>,
    // A sender of the status plane, declared second: the tail publishes
    // `Stopped` through it once everything the actor owned is dropped. On
    // return and on an abort while suspended it is dropped after every
    // parameter declared below it, so the plane stays open until the actor
    // value, the context and the queued messages are gone. On an abort
    // before the first poll the registry guard holds the plane open
    // the same way it holds the subtree. Each timer and stream forwarder
    // holds both signals too, so the plane and the subtree also outlive the
    // forwarders' futures.
    status: StatusTx,
    // Everything the actor owns, each in a `CaughtDrop`: dropped any way
    // other than by the tail below (an abort, before or after the first
    // poll, or the task's own unwind), its drop still catches every panic,
    // so drop glue never unwinds and the task ends as the abort or the
    // unwind does.
    actor_slot: CaughtDrop<A>,
    ctx_slot: CaughtDrop<ActorContext<A>>,
    mailbox_slot: QueueSlot<MailRx<ActorEnvelope<A>>>,
    priority_slot: QueueSlot<mpsc::Receiver<ActorEnvelope<A>>>,
    // Declared right after the queues, never a local of the body: on an abort
    // while suspended, or this task's own unwind, the parameters drop last to
    // first, so it writes the abort reason after `ack` and `registry_guard`
    // (the name is free) and before the queued messages, the context and the
    // value drop. A local would drop before `registry_guard`, and a reader of
    // the reason could meet the name still registered. On return the seal
    // wrote the reason first and it writes nothing.
    _abort_reason: AbortReason,
    system_rx: SystemPlane,
    stop_rx: LaneRx,
    // Clears the turn in flight when the task ends on any path.
    turn_clock: TurnGuard,
    // Declared after everything the actor owns: on an abort before the first
    // poll it frees the name, then releases its status sender and its
    // subtree hold, after the value, the context and the queued messages
    // are dropped.
    registry_guard: Option<RegistryGuard>,
    // The start ack, in a guard: the task's own unwind drops it with the other
    // parameters, and the caller's waker it wakes then must not add a second
    // panic to the unwind.
    ack: AnswerHold<Result<(), ActorError>>,
    // What the run loop needs to know about the runtime its task runs on, read
    // once at the spawn from the handle the task is spawned on (the placement
    // runtime when the spawn names one): how many worker threads it has, and
    // whether it is a current-thread runtime. The two are separate facts: a
    // multi-thread runtime with one worker has one worker and is not a
    // current-thread runtime. Declared last and plain values: they own
    // nothing, so no drop order depends on them.
    workers: usize,
    same_thread: bool,
}

/// The size of the run future of an actor type, for the size test.
#[cfg(test)]
#[cfg_attr(
    not(all(target_arch = "x86_64", target_os = "linux")),
    allow(dead_code)
)]
pub(crate) fn run_future_size<A: Actor>() -> usize {
    fn size_of_output<P, F>(_: impl FnOnce(P) -> F) -> usize {
        std::mem::size_of::<F>()
    }
    size_of_output(run_actor::<A>)
}

async fn run_actor<A: Actor>(parts: Box<RunParts<A>>) -> StopReason {
    // Moved out of the box field by field, in declaration order; the empty
    // box is freed at the end of the block.
    let RunParts {
        _subtree,
        status,
        mut actor_slot,
        mut ctx_slot,
        mut mailbox_slot,
        mut priority_slot,
        _abort_reason,
        mut system_rx,
        mut stop_rx,
        turn_clock,
        registry_guard,
        mut ack,
        workers,
        same_thread,
    } = {
        let parts = parts;
        *parts
    };
    // The loop reads one fact of the worker count, so it keeps one byte.
    let single_worker = workers <= 1;
    let actor = actor_slot.get_mut();
    let ctx = ctx_slot.get_mut();
    let mailbox = mailbox_slot.mailbox_mut();
    let priority = priority_slot.lane_mut();
    // When the start began, read only while a sink takes `ActorStarted`:
    // one plus the clock offset, or 0.
    let started_at = if armed_for(
        ctx.stop_lane().abort_record(),
        None,
        EventKind::ActorStarted,
    ) {
        now_nanos().saturating_add(1)
    } else {
        0
    };

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
        {
            let phase1 = {
                let turn = turn_clock.begin();
                let result =
                    catch_hook_t(turn_clock.caller_token(turn), || actor.pre_start(ctx)).await;
                if !turn.is_idle() {
                    let took = turn_clock.end_timed(turn);
                    push_turn(
                        ctx.stop_lane().abort_record(),
                        turn,
                        took,
                        HookKind::PreStart,
                        outcome_of(&result),
                    );
                }
                result
            };
            let phase1 = match phase1 {
                Ok(result) => result,
                Err(payload) => Err(ActorError::Panic(payload_into_string(payload))),
            };
            if let Err(err) = phase1 {
                break 'init Err(err);
            }
        }
        if init_kill_check(stop_rx.borrow().reason.as_ref()) == InitStep::KillNow {
            killed_in_init = true;
            break 'init Ok(());
        }

        // Phase 2: on_started initialization.
        // Scoped, so the hook's result is not kept across the strict wait.
        let strict = {
            let phase2 = {
                let turn = turn_clock.begin();
                let result =
                    catch_hook_t(turn_clock.caller_token(turn), || actor.on_started(ctx)).await;
                if !turn.is_idle() {
                    let took = turn_clock.end_timed(turn);
                    push_turn(
                        ctx.stop_lane().abort_record(),
                        turn,
                        took,
                        HookKind::OnStarted,
                        outcome_of(&result),
                    );
                }
                result
            };
            let phase2 = match phase2 {
                Ok(result) => result,
                Err(payload) => Err(ActorError::Panic(payload_into_string(payload))),
            };
            // The children spawned from here on are not part of the start.
            let strict = ctx.end_init_phase();
            if let Err(err) = phase2 {
                break 'init Err(err);
            }
            strict
        };

        // Strict start: the supervisor's own start waits for the children it
        // spawned while starting, and the first that fails fails it.
        if strict {
            match Box::pin(await_strict_children(ctx, &stop_rx)).await {
                StrictOutcome::Done => {}
                StrictOutcome::Killed => {
                    killed_in_init = true;
                    break 'init Ok(());
                }
                StrictOutcome::Failed(err) => break 'init Err(err),
            }
        }

        Ok(())
    };
    let started = init_result.is_ok() && !killed_in_init;
    // The first panic of a drop before the reason is final that the final
    // collection or the children's teardown meets (a child factory, the
    // supervision state); applied once the children are stopped.
    let mut teardown_panic: Option<String> = None;
    // The answer to the request whose handler failed, held to the lane's seal
    // (see `HeldAnswer`). A body local, not a parameter: an abort or an unwind
    // drops it with the body, before the name is freed.
    let mut held: HeldAnswer<A::Response> = HeldAnswer::new();

    let mut stop_reason = if killed_in_init {
        StopReason::Kill
    } else {
        match &init_result {
            Ok(()) => {
                ctx.set_status(ActorStatus::Running);
                // Success ack: right after the Running transition, before the
                // actor ever touches its mailbox.
                if let Some(tx) = ack.take() {
                    let _ = send_contained(tx, Ok(()));
                }
                if started_at != 0 {
                    let took = Duration::from_nanos(now_nanos().saturating_sub(started_at - 1));
                    push_about(
                        ctx.stop_lane().abort_record(),
                        None,
                        EventKind::ActorStarted,
                        |actor| TelemetryEvent::ActorStarted {
                            actor: actor.clone(),
                            took,
                        },
                    );
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
        // Cleared when the loop ends, on every path (an abort included).
        let lane_slot = SlotHold(ctx.stop_lane().abort_record().clone());
        let mut drain = ReplyDrain::new();
        let mut fp_skip: u32 = 0;
        // The sleep for the nearest threshold of the idle spell, made at the
        // first select entry that has one; nothing for an actor that sets no
        // deadline.
        let mut idle_sleep: Option<Pin<Box<tokio::time::Sleep>>> = None;
        // Whether the message `continue_with` asked for in a start hook still
        // has to run, before the first branch is read.
        let mut post_init = true;
        // The turn quantum (see `TURN_QUANTUM`): whether the next fast-path
        // take starts a run, when the run started, the last clock read, takes
        // since that read, and how many takes between reads.
        let mut run_fresh = true;
        let mut turn_start = std::time::Instant::now();
        let mut last_read = turn_start;
        let mut q_takes: u32 = 0;
        let mut q_stride: u32 = 8;
        // The takes of the run in flight, and this loop's copy of the handler
        // list that hot events are delivered from.
        let mut takes: u32 = 0;
        let mut hot = HotCache::new();
        // Ends the run in flight, if any, and starts over at the next take.
        // The event reaches the handlers on this task: every point that ends
        // a run owes nothing but the loop's next step.
        macro_rules! close_run {
            () => {
                if !run_fresh {
                    crate::telemetry::emit_hot(
                        &lane_slot.0,
                        None,
                        &mut hot,
                        EventKind::RunFinished,
                        |actor| TelemetryEvent::RunFinished {
                            actor: Cow::Borrowed(actor),
                            takes,
                            took: turn_start.elapsed(),
                        },
                    );
                }
                run_fresh = true;
            };
        }
        // `workers` and `same_thread` come from the spawn (see `RunParts`):
        // `same_thread` is whether a requester that is not a runtime task is
        // woken on this thread (see `ReplyDrain`).
        #[cfg(test)]
        loop_facts::record(ctx.actor_id(), workers, same_thread);
        // Whether the turn that just ran gave the worker up (a poll of its
        // handler or of a chain step returned pending). Reset by each
        // fast-path turn; the select's mailbox arm leaves it unread.
        let mut gave_back = false;
        // One plain turn of the loop, in the loop's own body: the handler
        // runs here, and the common end (an `Ok` that did not stop the actor)
        // is finished here, so the turn moves nothing the size of a
        // `ControlFlow<StopReason>` through memory. The signal envelope, the
        // end of a failed or self-stopping turn, the turn's hot events and
        // the chain of `continue_with` steps are out-of-line functions.
        // Evaluates to the one-byte `TurnEnd`; a stop has written
        // `stop_reason`.
        macro_rules! plain_turn {
            ($env:expr) => {{
                // The turn proper lives in its own block, so none of its
                // locals is held while a chain of `continue_with` steps runs.
                let mut end = 'run: {
                    let (payload, responder) = match { $env } {
                        Envelope::Message { payload, responder } => (payload, responder),
                        Envelope::Signal { tag, payload } => match open_signal(ctx, tag, payload) {
                            Ok(payload) => (payload, None),
                            Err(flow) => break 'run settle(flow, &mut stop_reason),
                        },
                    };
                    let mut responder = AnswerHold::new(responder);
                    ctx.clear_self_stop();
                    let turn = turn_clock.begin();
                    // The debug state, when the actor has any option on: the
                    // message is counted and logged inside the turn. An actor
                    // with none pays this one test.
                    if let Some(state) = ctx.debug_mut() {
                        state.message_in(responder.is_some(), false);
                    }
                    // The reply of a handler that returned `Ok` leaves the
                    // poll by itself and is all the turn reads back. Any
                    // other outcome (an `Err`, a panic) leaves boxed, so the
                    // common turn never copies the whole outcome out of the
                    // poll's result slot.
                    macro_rules! handler_step {
                        ($hook:expr) => {{
                            let mut handler = std::pin::pin!($hook);
                            std::future::poll_fn(|cx| match handler.as_mut().poll(cx) {
                                Poll::Pending => {
                                    gave_back = true;
                                    Poll::Pending
                                }
                                Poll::Ready(Ok(Ok(response))) => Poll::Ready(Ok(response)),
                                Poll::Ready(other) => Poll::Ready(Err(Box::new(other))),
                            })
                            .await
                        }};
                    }
                    // One poll site: the caller token is `0`, and sets
                    // nothing, for a turn that records no wait (two sites
                    // would keep the responder and the turn's mode live
                    // across two suspend points, which the compiler then
                    // places in every state of the loop's future).
                    let step = handler_step!(catch_hook_t(turn_clock.caller_token(turn), || {
                        actor.handle(payload, ctx)
                    }));
                    let took = if turn.is_idle() {
                        None
                    } else {
                        turn_clock.end_timed(turn)
                    };
                    #[cfg(test)]
                    end_hooks::run(end_hooks::TURN, ctx.actor_id());
                    match step {
                        Ok(response) if !ctx.self_stop_set() => {
                            let mut out = None;
                            match deliver::<A>(&mut drain, &mut responder, response, &mut out) {
                                None => {
                                    // The reply is sent and the loop owes only
                                    // its next take: the handlers run here.
                                    if ctx.debug_on() || turn.armed(EventKind::TurnFinished) {
                                        turn_done_hot(ctx, &mut hot, turn, took, out);
                                    }
                                    TurnEnd::Next
                                }
                                Some(payload) => {
                                    push_turn(
                                        ctx.stop_lane().abort_record(),
                                        turn,
                                        took,
                                        HookKind::Handle,
                                        TurnOutcome::Panicked,
                                    );
                                    settle(undelivered_panic(ctx, payload), &mut stop_reason)
                                }
                            }
                        }
                        Ok(response) => settle(
                            finish_turn(
                                ctx,
                                responder,
                                Ok(Ok(response)),
                                &mut drain,
                                &mut held,
                                turn,
                                took,
                                Some(&mut hot),
                            ),
                            &mut stop_reason,
                        ),
                        Err(other) => settle(
                            finish_turn(
                                ctx,
                                responder,
                                *other,
                                &mut drain,
                                &mut held,
                                turn,
                                took,
                                Some(&mut hot),
                            ),
                            &mut stop_reason,
                        ),
                    }
                };
                if ctx.has_continuation() {
                    let flow = flow_of(end, &mut stop_reason);
                    let flow = Box::pin(chain_after(
                        actor,
                        ctx,
                        flow,
                        &turn_clock,
                        &mut drain,
                        &mut held,
                        &mut gave_back,
                        &mut hot,
                    ))
                    .await;
                    end = settle(flow, &mut stop_reason);
                }
                end
            }};
        }
        loop {
            if post_init {
                post_init = false;
                // A start hook that stopped its own actor ends the chain
                // before its first step: the stop is read first.
                if ctx.take_self_stop() {
                    ctx.drop_continuation();
                }
                if ctx.has_continuation() {
                    let chain = Box::pin(continue_chain(
                        actor,
                        ctx,
                        ControlFlow::Continue(()),
                        &turn_clock,
                        &mut drain,
                        &mut held,
                        Some(&mut hot),
                    ))
                    .await;
                    if let ControlFlow::Break(reason) = chain {
                        stop_reason = reason;
                        break;
                    }
                }
            }
            let fp_off = fp_skip_step(&mut fp_skip);
            // A requester answered by this poll runs once the poll ends: end it
            // here when a backlog would otherwise keep the poll going.
            if drain.0 != 0 {
                if mailbox.is_empty() && priority.as_ref().is_none_or(|lane| lane.is_empty()) {
                    drain = ReplyDrain::new();
                } else if drain.step() {
                    // The run ends where the worker is given up.
                    close_run!();
                    #[cfg(test)]
                    hand_backs::bump(ctx.actor_id());
                    if same_thread {
                        // One thread: the requester (a `block_on` body among
                        // them) runs only once this task leaves the queue.
                        defer_armed(lane_slot.0.reader(), || {
                            hand_back_idle(&stop_rx, &system_rx, ctx, priority)
                        })
                        .await;
                    } else {
                        hand_back(lane_slot.0.reader(), || {
                            hand_back_idle(&stop_rx, &system_rx, ctx, priority)
                        })
                        .await;
                    }
                    // The hand-back gave the worker up: the run starts over.
                }
            }
            // Turn boundary fast path: every branch above the mailbox reads
            // idle by the check its own poll would make, in the select's
            // branch order (see `UpperBranch`), so the biased select below
            // would take the mailbox: take it here without registering on
            // the branches above. Anything else, an empty mailbox, or a
            // receive that is not ready now runs the select.
            if !tokio::task::coop::has_budget_remaining() {
                close_run!();
                if single_worker {
                    defer_armed(lane_slot.0.reader(), || {
                        hand_back_idle(&stop_rx, &system_rx, ctx, priority)
                    })
                    .await;
                } else {
                    requeue().await;
                }
            }
            if !fp_off
                && !mailbox_closed
                && !mailbox.is_empty()
                && !is_suspended(ctx)
                && upper_idle(&stop_rx, &system_rx, ctx, priority)
            {
                if let Some(env) = poll_now(mailbox).await {
                    #[cfg(test)]
                    fast_path_turns::bump();
                    if run_fresh {
                        run_fresh = false;
                        turn_start = std::time::Instant::now();
                        last_read = turn_start;
                        q_takes = 0;
                        takes = 0;
                    }
                    takes += 1;
                    gave_back = false;
                    match plain_turn!(env) {
                        TurnEnd::Next => {
                            if gave_back {
                                // The handler gave the worker back: the senders
                                // and the driver had their turn, and the run of
                                // takes starts over.
                                close_run!();
                                continue;
                            }
                            let mut pace = false;
                            q_takes += 1;
                            if q_takes >= q_stride {
                                q_takes = 0;
                                let now = std::time::Instant::now();
                                q_stride = if now.duration_since(last_read) > TURN_QUANTUM / 4 {
                                    1
                                } else {
                                    8
                                };
                                last_read = now;
                                if now.duration_since(turn_start) >= TURN_QUANTUM {
                                    pace = true;
                                }
                            }
                            if pace {
                                close_run!();
                                if single_worker {
                                    defer_armed(lane_slot.0.reader(), || {
                                        hand_back_idle(&stop_rx, &system_rx, ctx, priority)
                                    })
                                    .await;
                                } else {
                                    requeue().await;
                                }
                            }
                            continue;
                        }
                        TurnEnd::Stop => break,
                    }
                }
            }
            // The select may wait: the next fast-path take starts a new run.
            close_run!();
            q_takes = 0;
            // The nearest threshold of the idle spell, if the actor set any:
            // the spell starts here when the loop is about to wait with both
            // queues empty. The fast path never reaches this line.
            let mut idle_kind = None;
            if ctx.idle_armed() {
                let quiet =
                    mailbox.is_empty() && priority.as_ref().is_none_or(|lane| lane.is_empty());
                if let Some((at, kind)) = ctx.idle_next(Instant::now(), quiet) {
                    match idle_sleep.as_mut() {
                        Some(sleep) => {
                            // The timer driver is touched only for a new
                            // deadline.
                            if sleep.deadline() != at {
                                sleep.as_mut().reset(at);
                            }
                        }
                        None => idle_sleep = Some(Box::pin(tokio::time::sleep_until(at))),
                    }
                    idle_kind = Some(kind);
                }
            }
            // Read once per pass: only this loop changes it, between passes.
            let suspended = is_suspended(ctx);
            tokio::select! {
                biased;

                // Stop lane - highest priority, ahead of the system channel
                // and the mailbox alike. Only ever observed here, at the top
                // of the loop between two `handle` invocations, so the signal
                // itself never interrupts a callback in progress.
                () = tokio::task::coop::cooperative(lane_changed(&stop_rx, lane_slot.0.reader())) => {
                    let reason = stop_rx.borrow_and_update().reason.clone();
                    let Some(reason) = reason else { continue };
                    // Tier 3 (Kill): not vetoable; no callback begins once it is read
                    if matches!(reason, StopReason::Kill) {
                        stop_reason = StopReason::Kill;
                        break;
                    }
                    // Tier 1 (Graceful/ParentRequest/Shutdown): pre_stop gate,
                    // vetoable. A pre_stop panic lets the stop proceed as a
                    // Failure.
                    if StopSeverity::is_vetoable(&reason) {
                        let pre_stop_result = {
                            let turn = turn_clock.begin();
                            let result = catch_hook_t(turn_clock.caller_token(turn), || {
                                actor.pre_stop(&reason, ctx)
                            })
                            .await;
                            if !turn.is_idle() {
                                let took = turn_clock.end_timed(turn);
                                push_turn(
                                    ctx.stop_lane().abort_record(),
                                    turn,
                                    took,
                                    HookKind::PreStop,
                                    outcome_plain(&result),
                                );
                            }
                            result
                        };
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
                        // a panic alike (a Kill is not vetoable and begins no callback); a
                        // `pre_stop` panic is recorded and wins over an accept,
                        // a veto, or a forced reason raised meanwhile (the
                        // panic already happened, and masking it would
                        // silently drop a real failure), except that a
                        // `Failure` raised meanwhile stays the exit reason,
                        // as an earlier failure does for `on_stopped`;
                        // otherwise a forced
                        // reason (Failure, ParentKilled or Cancelled) wins over
                        // an accept or a veto, though `on_stopped` still runs
                        // for it.
                        let latest = stop_rx.borrow().reason.clone();
                        if matches!(latest, Some(StopReason::Kill)) {
                            // The panic of `pre_stop`, if any, is not
                            // recorded. Its payload is dropped here, inside a
                            // catch: the payload's own drop may panic, and a
                            // drop left to the `break` would unwind out of
                            // the task's poll, so the task would end by its
                            // own unwind instead of by the `Kill`.
                            if let Err(payload) = pre_stop_result {
                                discard(payload);
                            }
                            stop_reason = StopReason::Kill;
                            break;
                        }
                        match pre_stop_result {
                            Err(payload) => {
                                let err =
                                    ActorError::Panic(payload_into_string(payload));
                                ctx.record_failure(err.clone());
                                stop_reason = match latest {
                                    Some(standing @ StopReason::Failure(_)) => standing,
                                    _ => StopReason::Failure(err),
                                };
                                break;
                            }
                            Ok(accepted) => match latest {
                                Some(r) if StopSeverity::of(&r) == StopSeverity::Forced => {
                                    stop_reason = r;
                                    break;
                                }
                                // The lane only strengthens: a request of higher
                                // rank that landed while `pre_stop` ran is the
                                // reason the actor stops with, and `pre_stop`
                                // is not asked again.
                                _ if accepted => {
                                    stop_reason = latest.unwrap_or(reason);
                                    break;
                                }
                                // Actor rejected the stop; a later same-severity
                                // raise bumps the generation and re-fires this
                                // gate. The reply a self-stop held goes out now,
                                // since the actor keeps running.
                                _ => {
                                    // The chain does not run after a veto: a
                                    // message `pre_stop` asked for is dropped.
                                    ctx.drop_continuation();
                                    if let Some(payload) = held.release_now() {
                                        let err = ActorError::Panic(payload);
                                        ctx.record_failure(err.clone());
                                        stop_reason = StopReason::Failure(err);
                                        break;
                                    }
                                    continue;
                                }
                            },
                        }
                    }
                    // Tier 2 (Failure/ParentKilled/Cancelled): non-vetoable, on_stopped still runs
                    stop_reason = reason;
                    break;
                }

                // System channel - priority over user messages
                sys_msg = system_rx.recv() => {
                    // The message is consumed before the request is served, so
                    // the run loop's state holds no copy of it across the
                    // wait.
                    let request = match sys_msg {
                        Some(message) => match message {
                            SystemMessage::Supervise(request) => Some(request),
                            SystemMessage::GetStatus(request) => {
                                let info = sys_state::status_info(actor, ctx, &turn_clock);
                                answer_status(
                                    request,
                                    info,
                                    same_thread,
                                    &mut fp_skip,
                                    &mut drain,
                                );
                                None
                            }
                            SystemMessage::Signals => {
                                crate::actor::link::drain_signals(ctx);
                                None
                            }
                            SystemMessage::Sys(command) => {
                                match sys_serve::serve(*command, actor, ctx, &turn_clock) {
                                    ControlFlow::Continue(()) => None,
                                    ControlFlow::Break(reason) => {
                                        stop_reason = reason;
                                        break;
                                    }
                                }
                            }
                        },
                        None => break, // system channel closed
                    };
                    if let Some(request) = request {
                        match Box::pin(crate::actor::supervisor_ref::serve(ctx, request)).await {
                            ControlFlow::Continue(()) => {}
                            ControlFlow::Break(reason) => {
                                stop_reason = reason;
                                break;
                            }
                        }
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
                event = next_supervision_event(ctx), if !suspended => {
                    let verdict_woke = matches!(event, Some(SupervisionEvent::Verdict));
                    match event {
                        Some(SupervisionEvent::Restart(Ok((task, outcome)))) => {
                            if let Some(sup) = ctx.supervision_mut() {
                                sup.task_owner.remove(&task);
                            }
                            match Box::pin(handle_restart_outcome(actor, ctx, *outcome)).await {
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
                                match Box::pin(handle_restart_outcome(actor, ctx, outcome)).await {
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
                            match Box::pin(handle_child_stopped(actor, ctx, event)).await {
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
                    // through unresponsive members ends the supervisor, and
                    // so does a stop with no caller left to report its
                    // verdict: its own parent decides.
                    if let Some(reason) = check_verdicts(ctx, verdict_woke) {
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
                envelope = recv_optional(priority), if priority.is_some() && !suspended => {
                    match envelope {
                        Some(env) => {
                            let flow = dispatch_one(
                                actor,
                                ctx,
                                env,
                                &turn_clock,
                                &mut drain,
                                &mut held,
                                true,
                                Some(&mut hot),
                            )
                            .await;
                            let flow = if ctx.has_continuation() {
                                Box::pin(continue_chain(
                                    actor,
                                    ctx,
                                    flow,
                                    &turn_clock,
                                    &mut drain,
                                    &mut held,
                                    Some(&mut hot),
                                ))
                                .await
                            } else {
                                flow
                            };
                            match flow {
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
                //
                // The idle spell's threshold - the lowest branch - is polled
                // inside this one, after the mailbox found nothing, and only
                // for an actor that set a deadline: an actor that set none
                // passes no sleep, and its select has no branch for it. A fire
                // that finds anything queued does nothing and lets the loop
                // read it; one that finds a `Kill` stored ends the actor as
                // the lane would.
                wake = mail_or_idle(mailbox, mailbox_closed, idle_kind.and(idle_sleep.as_mut())), if !suspended && (!mailbox_closed || idle_kind.is_some()) => {
                    match wake {
                        Wake::Idle => {
                            let Some(kind) = idle_kind else { continue };
                            if !mailbox.is_empty() || priority.as_ref().is_some_and(|lane| !lane.is_empty()) {
                                continue;
                            }
                            if kill_stored(&stop_rx) {
                                stop_reason = StopReason::Kill;
                                break;
                            }
                            match Box::pin(fire_idle(actor, ctx, kind, &turn_clock, &mut drain, &mut held, Some(&mut hot))).await {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(reason) => {
                                    stop_reason = reason;
                                    break;
                                }
                            }
                        }
                        Wake::Mail(env) => {
                            if plain_turn!(env) == TurnEnd::Stop {
                                break;
                            }
                        }
                        Wake::Closed => {
                            // The mailbox has ended: nothing is queued and no
                            // sender is left. Dropping outside handles does
                            // not end it, since the context holds a handle of
                            // its own. Exit only if the priority lane is also
                            // gone (or never existed); otherwise it alone
                            // still justifies running.
                            if priority.is_none() {
                                break;
                            }
                            mailbox_closed = true;
                        }
                    }
                }
            }
        }

        // The run still open when the loop exits ends before the stop
        // sequence, which it must not wait for.
        if !run_fresh {
            push_run(&lane_slot.0, takes, turn_start);
        }
        // The log keeps the reason the loop ended with.
        if let Some(state) = ctx.debug_mut() {
            state.stopping(&stop_reason);
        }
        drop(hot);
        drop(lane_slot);
        // A message a callback asked to handle next is not handled once the
        // loop has ended.
        ctx.drop_continuation();
        #[cfg(test)]
        end_hooks::pause(end_hooks::LOOP_ENDED, ctx.actor_id()).await;

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
                // Boxed: only a supervisor's teardown pays for this state.
                match Box::pin(final_drain(
                    actor,
                    ctx,
                    &stop_rx,
                    &stop_reason,
                    &mut teardown_panic,
                ))
                .await
                {
                    FinalOutcome::Killed => stop_reason = StopReason::Kill,
                    FinalOutcome::Done { pending } => pending_exit = pending,
                }
            } else {
                tokio::task::yield_now().await;
            }
            #[cfg(test)]
            end_hooks::pause(end_hooks::FINAL_DONE, ctx.actor_id()).await;
            // The notices of the monitors this actor holds, owed since the
            // watched actors ended, are delivered after a supervisor's
            // child reports: the death plane outranks them.
            if !matches!(stop_reason, StopReason::Kill)
                && ctx.has_end_state()
                && (ctx.monitor_count() != 0 || ctx.link_work())
            {
                match Box::pin(monitor_drain(
                    actor,
                    ctx,
                    &stop_rx,
                    &stop_reason,
                    &turn_clock,
                    &mut *mailbox,
                    priority.as_mut(),
                    &mut drain,
                    &mut held,
                ))
                .await
                {
                    FinalOutcome::Killed => stop_reason = StopReason::Kill,
                    FinalOutcome::Done { pending } => {
                        if !matches!(pending_exit, Some(StopReason::Failure(_))) {
                            pending_exit = pending.or(pending_exit);
                        }
                    }
                }
            }
        }
        #[cfg(test)]
        end_hooks::pause(end_hooks::STAGE_DONE, ctx.actor_id()).await;
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
            let stopped = {
                let turn = turn_clock.begin();
                let result = catch_hook_t(turn_clock.caller_token(turn), || {
                    actor.on_stopped(&stop_reason, ctx)
                })
                .await;
                if !turn.is_idle() {
                    let took = turn_clock.end_timed(turn);
                    push_turn(
                        ctx.stop_lane().abort_record(),
                        turn,
                        took,
                        HookKind::OnStopped,
                        outcome_of(&result),
                    );
                }
                result
            };
            // A failed final delivery makes the exit reason a failure once
            // `on_stopped` has returned; `on_stopped`'s own result is applied
            // after it, the same way.
            if let Some(pending) = pending_exit {
                stop_reason = pending;
            }
            match stopped {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    // A cleanup error must not mask the original failure.
                    if !matches!(stop_reason, StopReason::Failure(_)) {
                        stop_reason = StopReason::Failure(err);
                    }
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
    ctx.stop_lane().close_to_non_kill();

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
        // The lane is tokio's bounded channel, and closing it wakes the
        // senders parked for room, whose wakers belong to their own tasks: a
        // panic of one stays in this call, and the reason is still the one
        // decided. Tokio ends its wake at that waker, so the senders behind it
        // may stay unwoken, and their futures find the lane closed at their
        // next poll.
        let who = ctx.actor_id().as_who();
        contain_waker_panic("priority lane", Some(who), || priority.close());
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
    let owner = ctx.owner();
    // Boxed, and skipped without a supervision state, where it returns
    // `Done` at once: only a supervisor's teardown pays for this state.
    if !matches!(stop_reason, StopReason::Kill)
        && ctx.supervision_ref().is_some()
        && Box::pin(stop_all_children(ctx, &mut stop_rx, &mut teardown_panic)).await
            == Teardown::Killed
    {
        stop_reason = StopReason::Kill;
    }
    if matches!(stop_reason, StopReason::Kill) {
        if let Some(sup) = ctx.take_supervision() {
            keep_first_of(&mut teardown_panic, end_supervision(sup, &owner), &owner);
        }
    }
    if let Some(payload) = teardown_panic {
        end_drop_panicked(
            &mut stop_reason,
            payload,
            &owner,
            ctx.stop_lane().abort_record(),
        );
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
    #[cfg(test)]
    end_hooks::run(end_hooks::GUARD_RELEASED, &owner);

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
    #[cfg(test)]
    end_hooks::pause(end_hooks::TEARDOWN_DONE, &owner).await;
    let end_panic = ctx.end_forwarders().await;
    // The one read of the signal maps' panics: every forwarder is ended and
    // joined, so no map function runs from here on, and the reason is not
    // yet chosen. It covers every window since the loop ended (the final
    // child reports, the stage's deliveries, `on_stopped`, the children's
    // stop), whichever signal's map panicked.
    collect_map_panic(ctx, &mut stop_reason);
    if let Some(payload) = end_panic {
        end_drop_panicked(
            &mut stop_reason,
            payload,
            &owner,
            ctx.stop_lane().abort_record(),
        );
    }

    // Seal the lane: every `Kill` a caller was told was accepted was either
    // observed above or is read here, and every request from here on is
    // refused with `Closed`, just before the queued messages and the actor
    // value are dropped. The receiver is dropped with it.
    // The cause a `Kill` carries comes back with it, read in the same step:
    // every classification above and below the seal sees `Kill`, and only
    // the final reason is the cause.
    let (sealed_reason, kill_cause) = ctx.stop_lane().seal_caused();
    if let Some(StopReason::Kill) = sealed_reason {
        stop_reason = StopReason::Kill;
    }
    drop(stop_rx);

    // The reason is final here and the name is free. The answer to the
    // request whose handler failed goes out now, unless a `Kill` ended the
    // actor: a caller that looks the actor up by name on reading the answer
    // finds nothing or the next incarnation, never this one. It is sent before
    // the reason is written, so a caller that finds its queue closed reads the
    // answer after the reason (`ActorHandle::send`), and a `Kill` accepted
    // before the seal is the reason the caller reads.
    if let Some(payload) = held.deliver(&stop_reason) {
        end_drop_panicked(
            &mut stop_reason,
            payload,
            &owner,
            ctx.stop_lane().abort_record(),
        );
    }
    #[cfg(test)]
    end_hooks::run(end_hooks::ANSWER, &owner);

    // The reason is final here, written into the status plane once: a panic
    // in a forwarder's drop at the end (see above) already made any reason
    // but a `Kill` or a `Failure` a failure, and the seal's `Kill` is read.
    // Readers of the plane (a request whose reply is dropped, `wait_stopped`,
    // a stop report) read this reason, as the task's return does. A `Kill`
    // raised for an exit signal ends the actor with that signal's reason.
    // Below this point `stop_reason` still says `Kill` for the classifications
    // that read the kind of the end (the init ack, the crash report), and
    // `ended_with` is the value the plane, the end event, the log and the
    // task's return carry.
    let cause_reason = final_cause(&stop_reason, kill_cause);
    let ended_with = cause_reason.as_ref().unwrap_or(&stop_reason);
    let ended = write_final_reason(&status, ended_with)
        // This call wrote the final reason: the end event follows the
        // writer, after the name is free and the lane sealed, with no guard
        // held.
        .then(|| emit_actor_ended(None, ctx.stop_lane().abort_record(), ended_with))
        .flatten();
    #[cfg(feature = "tracing")]
    let fields = crate::telemetry::LogFields::released(ended, ctx.stop_lane().abort_record());
    #[cfg(not(feature = "tracing"))]
    drop(ended);

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
        match init_result {
            Err(err) if !matches!(stop_reason, StopReason::Kill) => {
                let _ = send_contained(tx, Err(err));
            }
            // A failed init that a `Kill` reached before the seal: the ack is
            // dropped unanswered. The drop wakes the spawn's caller, whose
            // waker belongs to the caller's task, and a panic of it stays out
            // of this task, which ends with the reason it has.
            Err(err) => {
                #[cfg(feature = "tracing")]
                caught_log!(warn,
                    actor_id = %owner,
                    actor_name = fields.name(),
                    incarnation = fields.incarnation(),
                    system = fields.system(),
                    error = %fields.text(&err.to_string()),
                    "the actor's initialization failed and a Kill reached its end before the reason was final; the spawn reads Killed"
                );
                #[cfg(not(feature = "tracing"))]
                let _ = err;
                contain_waker_panic("start ack", Some(owner.id().as_who()), move || drop(tx));
            }
            // An init a `Kill` cut before it began: the ack is dropped
            // unanswered, contained the same way.
            Ok(()) => {
                contain_waker_panic("start ack", Some(owner.id().as_who()), move || drop(tx));
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
    let record = ctx.stop_lane().abort_record();
    let mut late_panic = mailbox_slot.end(Some(record));
    keep_first_of(&mut late_panic, priority_slot.end(Some(record)), &owner);
    let crash = reports::crash_text(
        actor_slot.peek(),
        &stop_reason,
        record,
        ctx.debug_ring(),
        &turn_clock,
    );
    keep_first_of(&mut late_panic, actor_slot.end(), &owner);
    keep_first_of(&mut late_panic, ctx_slot.end(), &owner);
    #[cfg(feature = "tracing")]
    if let Some(payload) = &late_panic {
        caught_log!(warn,
            actor_id = %owner,
            actor_name = fields.name(),
            incarnation = fields.incarnation(),
            system = fields.system(),
            reason = %fields.reason_text(ended_with),
            panic = %fields.panic_text(payload),
            "a drop after the actor's reason was final panicked; the reason stands"
        );
    }
    #[cfg(not(feature = "tracing"))]
    let _ = late_panic;
    publish_stopped(&status);
    #[cfg(feature = "tracing")]
    reports::crash_log(crash, &owner, &fields, &stop_reason);
    #[cfg(not(feature = "tracing"))]
    reports::crash_log(crash, &owner);

    #[cfg(feature = "tracing")]
    caught_log!(info,
        actor_id = %owner,
        actor_name = fields.name(),
        incarnation = fields.incarnation(),
        system = fields.system(),
        reason = %fields.reason_text(ended_with),
        "Actor stopped"
    );

    cause_reason.unwrap_or(stop_reason)
}

/// The run loop's hold on its waker slot: clears the slot when it drops, so
/// a waker registered by the loop never outlives the loop.
struct SlotHold(crate::actor::subtree::AbortRecord);

impl Drop for SlotHold {
    fn drop(&mut self) {
        self.0.reader().clear();
    }
}

/// The reason, `Failure(Panic(UNCAUGHT_PANIC))`, written for a task whose own
/// poll unwound past every catch: every call of user code is caught, so only
/// the crate's own code unwinds there. The task ends in the order of an abort
/// while suspended, with this reason instead of `Kill`: its status plane, the
/// runner or restart attempt that joins it, and an awaited spawn whose
/// initialization the unwind cut (as `SpawnError::Init`) read it. The end is
/// recorded on the status plane as a panic with this message. With the
/// `tracing` feature the supervisor that joins the task also logs the payload
/// in a warning; without it the crate does not log the payload. A process
/// that wants every panic payload in its log installs a hook with
/// `std::panic::set_hook`.
pub(crate) const UNCAUGHT_PANIC: &str = "the actor's task panicked outside every catch";

/// Set while this actor task's own future is being polled, and cleared after
/// each poll, an unwind included ([`MarkedRun`]). One per task: one thread
/// may poll or drop another actor's task inside a callback. A drop of the
/// task's state that writes the reason reads it to tell this task's own
/// unwind, inside its poll, from an abort, a shutdown, or a runtime dropped
/// during an unrelated unwind, which drop the task outside its poll.
///
/// Every access is `Relaxed`. Every clone lives inside the task's one future
/// and none leaves it, so each read is either on the thread that is polling
/// the task, where it sees that thread's own store, or in the future's drop
/// glue outside a poll, which the `Send` contract of the future orders after
/// the last poll's clearing store. No other thread reads the flag, and
/// nothing waits on it, so no stronger order has a reader to serve.
#[derive(Clone, Default)]
pub(crate) struct TaskMarker(Arc<AtomicBool>);

impl TaskMarker {
    /// The reason of a task dropped before its reason was written: a panic
    /// when this task's own poll is unwinding, `Kill` otherwise.
    pub(crate) fn abort_reason(&self) -> StopReason {
        if self.0.load(Ordering::Relaxed) && std::thread::panicking() {
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
///
/// It also records, for the length of each poll, which incarnation the thread
/// is running and in which task (`PollScope`), so a request the actor sends to
/// itself from its own task is refused (`AskError::CallingSelf`). The record
/// is put back by a guard on every path out of the poll. The future's state
/// drops inside the same record when the future is dropped before it ended (an
/// abort, a runtime shutting down, an unwind), so a drop that calls back into
/// the actor is refused as well.
struct MarkedRun<F> {
    marker: TaskMarker,
    /// The incarnation this future runs.
    incarnation: Incarnation,
    /// The id of the task that polls this future, read at its first poll
    /// inside a task and never again: a future is polled by one task for its
    /// whole life.
    task: Option<tokio::task::Id>,
    /// Taken when the future ends, inside the record of its last poll.
    inner: Option<Pin<Box<F>>>,
}

impl<F: Future> MarkedRun<F> {
    fn new(marker: TaskMarker, incarnation: Incarnation, inner: F) -> Self {
        Self {
            marker,
            incarnation,
            task: None,
            inner: Some(Box::pin(inner)),
        }
    }
}

impl<F> Drop for MarkedRun<F> {
    fn drop(&mut self) {
        // An ended future has nothing left. One cut short drops its state
        // here, in the record of its incarnation and its task.
        if let Some(inner) = self.inner.take() {
            // A future that was never polled has read no id yet: the drop runs
            // in the task that was to poll it.
            let task = self.task.or_else(tokio::task::try_id);
            let _scope = PollScope::enter(self.incarnation, task);
            drop(inner);
        }
    }
}

/// Clears a task's marker when it drops.
struct ClearMarker<'a>(&'a AtomicBool);

impl Drop for ClearMarker<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl<F: Future> Future for MarkedRun<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        if this.task.is_none() {
            this.task = tokio::task::try_id();
        }
        let _scope = PollScope::enter(this.incarnation, this.task);
        this.marker.0.store(true, Ordering::Relaxed);
        let _clear = ClearMarker(&this.marker.0);
        let inner = this
            .inner
            .as_mut()
            .expect("a future is not polled after it ended");
        let output = std::task::ready!(inner.as_mut().poll(cx));
        // Its state drops here, inside the record and the marker.
        this.inner = None;
        Poll::Ready(output)
    }
}

/// Writes the abort reason into the status plane when it drops and no reason
/// is written: `Kill`, or a panic for this task's own unwind
/// ([`TaskMarker::abort_reason`]). See `run_actor`'s `_abort_reason`.
pub(crate) struct AbortReason {
    status: StatusTx,
    marker: TaskMarker,
    /// The actor, for the hook tests observe the write through. A build that
    /// is not a test build keeps no id here, so a spawn takes no reference
    /// count for it.
    #[cfg(test)]
    owner: ActorId,
}

impl AbortReason {
    #[cfg(test)]
    fn new(status: StatusTx, marker: TaskMarker, owner: ActorId) -> Self {
        Self {
            status,
            marker,
            owner,
        }
    }

    #[cfg(not(test))]
    fn new(status: StatusTx, marker: TaskMarker) -> Self {
        Self { status, marker }
    }
}

impl Drop for AbortReason {
    fn drop(&mut self) {
        if self.status.reason_written() || self.status.borrow().reason.is_some() {
            return;
        }
        #[cfg(test)]
        end_hooks::run(end_hooks::REASON, &self.owner);
        let reason = aborted_reason(&self.status, &self.marker);
        if crate::telemetry::engaged_for(self.status.abort_record(), None) {
            self.write_reason_noted(reason);
            return;
        }
        StatusValue::write_reason(&self.status, reason, false);
    }
}

impl AbortReason {
    /// The tail of the drop for a system that takes telemetry events or holds
    /// an identity: the reason is copied for the end event.
    #[inline(never)]
    fn write_reason_noted(&self, reason: StopReason) {
        let kept = reason.clone();
        if StatusValue::write_reason(&self.status, reason, false) {
            emit_actor_ended(None, self.status.abort_record(), &kept);
        }
    }
}

/// Hooks tests attach by point and actor id: [`REASON`](end_hooks::REASON)
/// runs just before an abort guard writes its reason,
/// [`NAME`](end_hooks::NAME) just before a registry guard frees the name,
/// [`NAME_FREED`](end_hooks::NAME_FREED) just after it freed the name,
/// [`GUARD_RELEASED`](end_hooks::GUARD_RELEASED) just after the guard and
/// the system it held were released, before the stop lane is sealed,
/// [`ANSWER`](end_hooks::ANSWER) just after the answer to a failed request is
/// sent, before the reason is written,
/// [`WALK`](end_hooks::WALK) in a supervisor's teardown, before it orders a
/// child that is still in its registry to stop (keyed by the supervisor),
/// [`TURN`](end_hooks::TURN) in a turn, after the handler returned and before
/// the turn applies its outcome, while the request's sender is still held,
/// [`CLAIM`](end_hooks::CLAIM) just before a supervisor claims a child's
/// death, [`SETTLED`](end_hooks::SETTLED) just after a restart attempt of the
/// child settled, [`REPORTED_WAIT`](end_hooks::REPORTED_WAIT) when a
/// supervisor's teardown begins to wait for the incarnations whose ends its
/// final collection took from their status planes (keyed by the supervisor),
/// [`ATTEMPT_WAIT`](end_hooks::ATTEMPT_WAIT) when a supervisor's final
/// collection begins to wait for restart attempts (keyed by the supervisor),
/// and [`PLANE_REPORT`](end_hooks::PLANE_REPORT) just after the final
/// collection took a child's death from its status plane (keyed by the
/// child). A hold attached at [`ACK`](end_hooks::ACK) makes a restart attempt
/// wait, after it received its child's acknowledgment and before it reads its
/// stop order, until the hold is notified (keyed by the child).
#[cfg(test)]
pub(crate) mod end_hooks {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    use crate::types::ActorId;

    pub(crate) const REASON: &str = "reason";
    pub(crate) const NAME: &str = "name";
    pub(crate) const NAME_FREED: &str = "name freed";
    pub(crate) const GUARD_RELEASED: &str = "guard released";
    pub(crate) const ANSWER: &str = "answer";
    pub(crate) const WALK: &str = "walk";
    pub(crate) const TURN: &str = "turn";
    pub(crate) const PG_MARKED: &str = "pg marked";
    /// The message loop ended, before the supervisor's final child reports.
    pub(crate) const LOOP_ENDED: &str = "loop ended";
    /// The final child reports are delivered, before the monitor stage.
    pub(crate) const FINAL_DONE: &str = "final done";
    /// The monitor stage is done, before `on_stopped`.
    pub(crate) const STAGE_DONE: &str = "stage done";
    /// The children are stopped, before the forwarders end.
    pub(crate) const TEARDOWN_DONE: &str = "teardown done";
    pub(crate) const CLAIM: &str = "claim";
    pub(crate) const SETTLED: &str = "settled";
    pub(crate) const REPORTED_WAIT: &str = "reported wait";
    pub(crate) const ATTEMPT_WAIT: &str = "attempt wait";
    pub(crate) const PLANE_REPORT: &str = "plane report";
    pub(crate) const ACK: &str = "ack";
    /// A hold that parks a restart attempt after its incarnation is spawned
    /// and published, before the attempt goes on (keyed by the child).
    pub(crate) const SPAWNED: &str = "spawned";
    /// A hold that parks a restart attempt's acknowledgment branch before it
    /// reads the channel (keyed by the child), so the join branch can read
    /// the incarnation's end first.
    pub(crate) const ACK_GAP: &str = "ack gap";
    /// A restart attempt's join branch found the acknowledgment stored
    /// (keyed by the child).
    pub(crate) const ACK_LATE: &str = "ack late";
    /// A task of the death plane that followed a child's incarnation failed
    /// instead of returning its outcome, and its supervisor accounts for the
    /// loss (keyed by the child).
    pub(crate) const RUNNER_LOST: &str = "runner lost";

    type Hook = Box<dyn Fn() + Send + Sync>;

    fn hooks() -> &'static Mutex<HashMap<(&'static str, ActorId), Hook>> {
        static HOOKS: OnceLock<Mutex<HashMap<(&'static str, ActorId), Hook>>> = OnceLock::new();
        HOOKS.get_or_init(Default::default)
    }

    /// How many hooks are attached at [`TURN`]. Every turn of every actor
    /// reads it, so a turn with none attached takes no lock.
    static TURN_HOOKS: AtomicUsize = AtomicUsize::new(0);

    /// Runs `hook` once, at `point` of the actor `id`.
    pub(crate) fn attach(
        point: &'static str,
        id: ActorId,
        hook: impl Fn() + Send + Sync + 'static,
    ) {
        let replaced = hooks().lock().unwrap().insert((point, id), Box::new(hook));
        if point == TURN && replaced.is_none() {
            TURN_HOOKS.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A point the actor waits at: it signals `reached`, then waits for a
    /// permit in `release`.
    pub(crate) struct Gate {
        pub(crate) reached: tokio::sync::Notify,
        pub(crate) release: tokio::sync::Semaphore,
    }

    impl Gate {
        pub(crate) fn new() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                reached: tokio::sync::Notify::new(),
                release: tokio::sync::Semaphore::new(0),
            })
        }

        /// Signals `reached`, then waits for a permit in `release`.
        pub(crate) async fn pass(&self) {
            self.reached.notify_one();
            if let Ok(permit) = self.release.acquire().await {
                permit.forget();
            }
        }
    }

    type Gates = Mutex<HashMap<(&'static str, ActorId), std::sync::Arc<Gate>>>;

    fn gates() -> &'static Gates {
        static GATES: OnceLock<Gates> = OnceLock::new();
        GATES.get_or_init(Default::default)
    }

    /// How many gates are attached, so a point with none takes no lock.
    static GATE_COUNT: AtomicUsize = AtomicUsize::new(0);

    /// Attaches a gate at `point` of the actor `id`; the actor waits there
    /// once.
    pub(crate) fn attach_gate(point: &'static str, id: ActorId) -> std::sync::Arc<Gate> {
        let gate = Gate::new();
        gates().lock().unwrap().insert((point, id), gate.clone());
        GATE_COUNT.fetch_add(1, Ordering::SeqCst);
        gate
    }

    /// Waits at the gate attached at `point` of the actor `id`, if any.
    pub(crate) async fn pause(point: &'static str, id: &ActorId) {
        if GATE_COUNT.load(Ordering::SeqCst) == 0 {
            return;
        }
        let gate = gates().lock().unwrap().remove(&(point, id.clone()));
        if let Some(gate) = gate {
            GATE_COUNT.fetch_sub(1, Ordering::SeqCst);
            gate.pass().await;
        }
    }

    type Hold = std::sync::Arc<tokio::sync::Notify>;

    fn holds() -> &'static Mutex<HashMap<(&'static str, ActorId), Hold>> {
        static HOLDS: OnceLock<Mutex<HashMap<(&'static str, ActorId), Hold>>> = OnceLock::new();
        HOLDS.get_or_init(Default::default)
    }

    /// Makes the task that reaches `point` of the actor `id` wait, once,
    /// until `hold` is notified.
    pub(crate) fn attach_hold(point: &'static str, id: ActorId, hold: Hold) {
        holds().lock().unwrap().insert((point, id), hold);
    }

    /// Waits on the hold attached at `point` of the actor `id`, if any.
    pub(crate) async fn wait_hold(point: &'static str, id: &ActorId) {
        let hold = holds().lock().unwrap().remove(&(point, id.clone()));
        if let Some(hold) = hold {
            hold.notified().await;
        }
    }

    /// Runs the hook attached at `point` of the actor `id`, if any, and says
    /// whether one ran.
    pub(crate) fn run(point: &'static str, id: &ActorId) -> bool {
        if point == TURN && TURN_HOOKS.load(Ordering::SeqCst) == 0 {
            return false;
        }
        let hook = hooks().lock().unwrap().remove(&(point, id.clone()));
        if let Some(hook) = hook {
            if point == TURN {
                TURN_HOOKS.fetch_sub(1, Ordering::SeqCst);
            }
            hook();
            return true;
        }
        false
    }
}

/// Runs `drop` and catches a panic it raises.
fn catch_drop(drop: impl FnOnce()) -> Result<(), String> {
    catch_sync(drop).map_err(payload_into_string)
}

/// The actor a drop belongs to, as the log lines of a drop panic name it: its
/// id, and whether it is sensitive, so a line about a panic whose text the
/// crate does not know the origin of still hides that text for a sensitive
/// actor. It is made from one incarnation's record (see [`Owner::of`]) or at
/// that incarnation's spawn, never from an id alone. The sensitive form holds
/// the id behind a pointer, which keeps the type the size of an `ActorId`.
#[derive(Clone)]
pub(crate) enum Owner {
    /// An actor whose failure text its log lines print.
    Open(ActorId),
    /// An actor whose failure text no log line prints.
    Sensitive(Arc<ActorId>),
}

impl Owner {
    /// The owner of an incarnation whose sensitivity the caller knows.
    #[inline]
    pub(crate) fn new(id: ActorId, sensitive: bool) -> Self {
        if sensitive {
            Self::Sensitive(Arc::new(id))
        } else {
            Self::Open(id)
        }
    }

    /// The owner of the incarnation `record` belongs to, named `id`.
    #[inline]
    pub(crate) fn of(id: &ActorId, record: &AbortRecord) -> Self {
        Self::new(
            id.clone(),
            record.sys_bits() & crate::actor::subtree::SYS_SENSITIVE != 0,
        )
    }

    /// The actor's id.
    pub(crate) fn id(&self) -> &ActorId {
        match self {
            Self::Open(id) => id,
            Self::Sensitive(id) => id,
        }
    }

    /// The identity fields of a line about this actor that carries a panic's
    /// text.
    #[cfg(feature = "tracing")]
    pub(crate) fn fields(&self) -> crate::telemetry::LogFields<'static> {
        crate::telemetry::LogFields::of_sensitive(matches!(self, Self::Sensitive(_)))
    }
}

impl std::ops::Deref for Owner {
    type Target = ActorId;

    fn deref(&self) -> &ActorId {
        self.id()
    }
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.id().fmt(f)
    }
}

impl std::fmt::Debug for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.id().fmt(f)
    }
}

/// Keeps the first of two panics of one step of the actor `owner`'s end. A
/// later one is discarded, and with the `tracing` feature it is logged,
/// naming the actor, so every panic leaves a record.
pub(crate) fn keep_first_of(first: &mut Option<String>, next: Option<String>, owner: &Owner) {
    match (first.is_some(), next) {
        (false, next) => *first = next,
        (true, Some(discarded)) => log_discarded(&discarded, owner),
        (true, None) => {}
    }
}

/// Logs, with the `tracing` feature, a panic a step discards because it
/// keeps an earlier one.
pub(crate) fn log_discarded(payload: &str, owner: &Owner) {
    #[cfg(feature = "tracing")]
    caught_log!(warn,
        actor_id = %owner,
        panic = %owner.fields().panic_text(payload),
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
    end: fn(T, &Owner) -> Option<String>,
    /// The actor that owns the value, passed to `end` on every path (the
    /// end of the task, an abort or an unwind) so each log line names it.
    owner: Owner,
}

impl<T> CaughtDrop<T> {
    pub(crate) fn new(value: T, end: fn(T, &Owner) -> Option<String>, owner: Owner) -> Self {
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

    /// The value, while it is held.
    fn peek(&self) -> Option<&T> {
        self.value.as_ref()
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
                panic = %self.owner.fields().panic_text(payload),
                "a drop at the abort or the unwind of the actor's task panicked; the task ends as the abort or the unwind does"
            );
        }
        #[cfg(not(feature = "tracing"))]
        let _ = (panic, &self.owner);
    }
}

/// Drops a value, catching a panic its drop raises.
pub(crate) fn end_value<T>(value: T) -> Option<String> {
    catch_drop(move || drop(value)).err()
}

/// An actor's mailbox or priority lane as the run loop holds it, with the
/// system whose helper slot takes the receiver if the queue ends with a slot
/// still out. Dropped any way other than by [`end`](Self::end) (an abort or
/// the task's own unwind), it ends the queue the same way, every panic
/// caught, like a [`CaughtDrop`].
pub(crate) struct QueueSlot<R: QueueRx> {
    rx: Option<R>,
    /// Held only while there is a receiver to hand off.
    system: Option<Arc<ActorSystem>>,
    owner: Owner,
}

impl<R: QueueRx> QueueSlot<R> {
    pub(crate) fn new(rx: Option<R>, system: &Arc<ActorSystem>, owner: Owner) -> Self {
        let system = rx.is_some().then(|| system.clone());
        Self { rx, system, owner }
    }

    /// The mailbox's receiver, held until the end.
    fn mailbox_mut(&mut self) -> &mut R {
        self.rx.as_mut().expect("the mailbox is held until its end")
    }

    /// The lane's receiver, which the loop lets go of when every sender of
    /// the lane is gone, and which an actor without the lane never had.
    fn lane_mut(&mut self) -> &mut Option<R> {
        &mut self.rx
    }

    /// Ends the queue now; returns the first panic a drop raised. `record` is
    /// the actor's abort record when the caller has it.
    fn end(&mut self, record: Option<&AbortRecord>) -> Option<String> {
        let rx = self.rx.take()?;
        let panic = match self.system.take() {
            Some(system) => end_queue_late(rx, &system, &self.owner, record),
            None => end_queue(rx, &self.owner),
        };
        #[cfg(test)]
        crate::actor::handle::late_slot_tests::between_queue_ends(&self.owner);
        panic
    }
}

impl<R: QueueRx> Drop for QueueSlot<R> {
    fn drop(&mut self) {
        let panic = self.end(None);
        #[cfg(feature = "tracing")]
        if let Some(payload) = &panic {
            caught_log!(warn,
                actor_id = %self.owner,
                panic = %self.owner.fields().panic_text(payload),
                "a drop at the abort or the unwind of the actor's task panicked; the task ends as the abort or the unwind does"
            );
        }
        #[cfg(not(feature = "tracing"))]
        let _ = panic;
    }
}

/// Closes a queue and drops the messages still in it one at a time, each
/// drop caught, so a second panicking message never panics inside the
/// unwinding of the first.
///
/// The close is followed by a full fence, then the drain. A requester pushes
/// its message and then reads whether the queue is closed behind the same
/// fence (see `ActorHandle::send`): in the order of the two fences, either
/// the requester's push comes before the end's fence and the drain sees the
/// message, or the end's close comes before the requester's fence and the
/// requester sees the queue closed. No request is left unseen by both.
///
/// The mailbox is covered the same way: its one queue is drained after the
/// end's fence, so a request racing the close is in the drain's reach or
/// sees the close.
///
/// Returns the first panic of the drain's drops and whether a value can still
/// arrive.
fn close_and_drain<R: QueueRx>(rx: &mut R, owner: &Owner) -> (Option<String>, bool) {
    // Closing a tokio channel wakes the senders parked for room, whose wakers
    // belong to their own tasks: a panic of one stays in this call, and the
    // drain below still runs. Tokio ends its wake at that waker, so the
    // senders behind it may stay unwoken and find the queue closed at their
    // next poll.
    contain_waker_panic("queue close", Some(owner.id().as_who()), || rx.close());
    std::sync::atomic::fence(Ordering::SeqCst);
    let mut first = None;
    loop {
        match rx.try_recv_one() {
            Ok(message) => keep_first_of(&mut first, end_value(message), owner),
            Err(open) => return (first, open),
        }
    }
}

/// Ends a queue that cannot have a slot out for a sender that is still to
/// push: closed, drained, dropped.
pub(crate) fn end_queue<R: QueueRx>(mut rx: R, owner: &Owner) -> Option<String> {
    let (mut first, _) = close_and_drain(&mut rx, owner);
    keep_first_of(&mut first, end_value(rx), owner);
    first
}

/// Ends a queue that senders may still push into through a slot reserved
/// before the close.
///
/// A request pushed through such a slot answers itself: its requester reads
/// the closed queue after its push and takes the actor's final reason from
/// the status plane (see `ActorHandle::send`), so no caller waits on this
/// end. What else arrives through a slot (a notification, a timer's message)
/// is dropped by the crate when the slot is used or given back, outside the end
/// order, and `wait_stopped` does not wait for it. The mailbox's queue
/// closes for good here, so a push from then on fails and its sender drops the
/// message in that push; a lane keeps taking what its permits send. While a
/// value can still arrive (a push already landing in the mailbox, a permit out
/// on a lane), the receiver goes to a helper task of the system that drops
/// what arrives. With no runtime to run that task, or a runtime that is
/// closing, the receiver drops here and a value that lands later is dropped
/// when the last sender is. On some Tokio releases
/// a blocked sender that is handed the slot the actor's last receive freed, and
/// is then dropped, does not wake the receiver, so the helper stays parked
/// until the queue's last sender is dropped.
pub(crate) fn end_queue_late<R: QueueRx>(
    mut rx: R,
    system: &ActorSystem,
    owner: &Owner,
    record: Option<&AbortRecord>,
) -> Option<String> {
    let (mut first, open) = close_and_drain(&mut rx, owner);
    // The queue is empty, and a value can still arrive only through a push
    // that is landing (the mailbox, closed for good) or a permit that is out
    // (a lane): hand the receiver to a task that drops what arrives.
    if open {
        if let Ok(rt) = Handle::try_current() {
            // The slot records the task's end, whichever way it ends.
            let task_owner = owner.clone();
            let helper_owner = HelperOwner::new(owner.id().clone(), record.cloned());
            drop(
                system.spawn_helper(&rt, HelperKind::LateDrain, Some(helper_owner), |end| {
                    drain_late(LateDrain::new(rx, task_owner, end))
                }),
            );
            return first;
        }
    }
    keep_first_of(&mut first, end_value(rx), owner);
    first
}

/// Closes a queue, then drops what it holds one value at a time, each drop
/// caught; returns the first panic.
fn end_remaining<R: QueueRx>(mut rx: R, owner: &Owner) -> Option<String> {
    let (mut first, _) = close_and_drain(&mut rx, owner);
    keep_first_of(&mut first, end_value(rx), owner);
    first
}

/// What the drain of a closed queue holds: the receiver, dropped under catch
/// however the task ends. If the task is dropped before it ends, the values it
/// still holds go with it unsettled, and the slot records their number.
struct LateDrain<R: QueueRx> {
    rx: CaughtDrop<R>,
    end: Option<EndGuard>,
}

impl<R: QueueRx> LateDrain<R> {
    fn new(rx: R, owner: Owner, end: EndGuard) -> Self {
        Self {
            rx: CaughtDrop::new(rx, end_remaining, owner),
            end: Some(end),
        }
    }
}

impl<R: QueueRx> Drop for LateDrain<R> {
    fn drop(&mut self) {
        // Runs before the fields drop, so the receiver still holds what the
        // task never took.
        if let Some(end) = self.end.take() {
            let dropped = self.rx.value.as_ref().map_or(0, QueueRx::queued);
            end.cancelled(dropped);
        }
    }
}

/// Drops what still arrives after its queue closed. On a lane: what a sender
/// pushes through a slot it reserved before the close, until every slot is
/// used or given back, or no sender is left. On the mailbox, whose end closed
/// its queue for good: the push that was landing, until none is.
/// It waits on the receiver alone, so it needs no timer and runs on a
/// runtime without a time driver. Every drop is caught and a panic is
/// logged with the actor's id, and whatever the receiver still holds when
/// the task ends, however it ends, is dropped under the same catch.
async fn drain_late<R: QueueRx>(mut late: LateDrain<R>) {
    let owner = late.rx.owner.clone();
    let rx = late.rx.get_mut();
    let drained = catch_callback(async {
        while let Some(message) = std::future::poll_fn(|cx| rx.poll_recv_one(cx)).await {
            if let Some(payload) = end_value(message) {
                log_late_drop(&payload, &owner);
            }
            if rx.free_units() == rx.all_units() {
                break;
            }
        }
    })
    .await;
    let end = late.end.take().expect("the guard is held until the end");
    match drained {
        Ok(()) => end.finished(),
        Err(payload) => {
            let payload = payload_into_string(payload);
            end.panicked(&payload);
        }
    }
}

/// Logs, with the `tracing` feature, a panic raised while dropping a value
/// that arrived through a slot reserved before its queue closed.
pub(crate) fn log_late_drop(payload: &str, owner: &Owner) {
    #[cfg(feature = "tracing")]
    caught_log!(warn,
        actor_id = %owner,
        panic = %owner.fields().panic_text(payload),
        "a drop of a value that arrived after its queue closed panicked; the panic is contained"
    );
    #[cfg(not(feature = "tracing"))]
    let _ = (payload, owner);
}

/// Drops a supervisor's state: its child factories one at a time, each drop
/// caught, then the rest.
fn end_supervision(mut sup: SupervisionState, owner: &Owner) -> Option<String> {
    let mut first = None;
    for (_, factory) in std::mem::take(&mut sup.restart_fns) {
        keep_first_of(&mut first, end_value(factory), owner);
    }
    keep_first_of(&mut first, end_value(sup), owner);
    first
}

/// Drops an actor's context: its supervision state first, as
/// [`end_supervision`] does, then the rest, each drop caught.
fn end_context<A: Actor>(mut ctx: ActorContext<A>, owner: &Owner) -> Option<String> {
    let mut first = ctx
        .take_supervision()
        .and_then(|sup| end_supervision(sup, owner));
    keep_first_of(&mut first, end_value(ctx), owner);
    first
}

/// The reason an actor ends with: the cause its `Kill` carries when one does,
/// otherwise the reason the run loop decided. A cause exists only with a
/// `Kill` on the lane, so every other reason passes through.
///
/// Returns the cause when it replaces the decided reason, and `None` when the
/// decided reason stands, so the common end copies nothing.
pub(crate) fn final_cause(
    decided: &StopReason,
    kill_cause: Option<StopReason>,
) -> Option<StopReason> {
    match (decided, kill_cause) {
        (StopReason::Kill, Some(cause)) => Some(cause),
        _ => None,
    }
}

/// The reason an actor ends with, as a value.
#[cfg(test)]
pub(crate) fn final_reason(decided: StopReason, kill_cause: Option<StopReason>) -> StopReason {
    final_cause(&decided, kill_cause).unwrap_or(decided)
}

/// The reason a task dropped before it wrote one is given: a panic when its
/// own poll is unwinding, else the cause the `Kill` on its lane carries, else
/// `Kill`. The one place the abort guard and the context drop read it.
pub(crate) fn aborted_reason(status: &StatusTx, marker: &TaskMarker) -> StopReason {
    match marker.abort_reason() {
        StopReason::Kill => status.record().kill_cause().unwrap_or(StopReason::Kill),
        panicked => panicked,
    }
}

/// Writes the actor's final reason into its status plane, once: a plane
/// that already carries a reason keeps it.
fn write_final_reason(status: &StatusTx, reason: &StopReason) -> bool {
    StatusValue::write_reason(status, reason.clone(), false)
}

/// The supervisor a registry entry names: `None` for a root, else the
/// spawning supervisor's id and incarnation number. A child spawned with no
/// supervisor in its birth names one the registry never holds, so nothing is
/// guessed for it.
fn registry_parent(is_root: bool, parent: Option<(&ActorId, u64)>) -> Option<(ActorId, u64)> {
    if is_root {
        return None;
    }
    Some(match parent {
        Some((id, seq)) => (id.clone(), seq),
        None => (ActorId::from(""), crate::telemetry::UNKNOWN_INCARNATION),
    })
}

/// Raises the end event of an incarnation whose final reason the caller just
/// wrote, in the system in hand or the one the record names. The event goes to
/// the pump, from the task's own end as from drop glue, so no handler runs
/// before the init ack, the mail drop and the `Stopped` publication that still
/// follow the reason. Whichever writer wins the reason is the only one that
/// calls this, so one incarnation raises one end event. Returns the identity
/// for the log lines that follow; an incarnation spawned with none raises
/// nothing.
#[inline]
pub(crate) fn emit_actor_ended(
    system: Option<&ActorSystem>,
    record: &AbortRecord,
    reason: &StopReason,
) -> Option<Arc<Identity>> {
    crate::telemetry::with_mode(record, system, |mode| {
        ended_event(mode, record, system, reason)
    })
    .flatten()
}

/// The part of [`emit_actor_ended`] that runs only while telemetry is on.
#[inline(never)]
fn ended_event(
    mode: &Arc<crate::telemetry::Mode>,
    record: &AbortRecord,
    system: Option<&ActorSystem>,
    reason: &StopReason,
) -> Option<Arc<Identity>> {
    if !mode.armed(EventKind::ActorEnded) {
        // No sink takes the end: only an identity already recorded is read,
        // for the log lines, and nothing is looked up.
        #[cfg(feature = "tracing")]
        return mode.find(record.key());
        #[cfg(not(feature = "tracing"))]
        return None;
    }
    let Some(identity) = crate::telemetry::identity_for(mode, record, system, None) else {
        // Nothing names the incarnation: the end event is lost, and the loss
        // is counted with the other drops.
        mode.pump.count_dropped();
        return None;
    };
    mode.push_owned(&identity, EventKind::ActorEnded, || {
        TelemetryEvent::ActorEnded {
            actor: identity.actor().clone(),
            reason: reason.clone(),
            lifetime: identity.lifetime(),
        }
    });
    Some(identity)
}

/// Publishes `Stopped` on a plane whose reason is already written.
pub(crate) fn publish_stopped(status: &StatusTx) {
    status.send_if_modified(|value| {
        value.status = ActorStatus::Stopped;
        true
    });
}

/// Reads the first panic of a signal map function (a monitor's, a link's trap
/// map or a group monitor's) that failed the actor, kept by the site that raised its
/// `Failure`, and makes it the exit reason unless a `Kill` or a `Failure`
/// already stands: the first failure stands. The raise on the stop lane
/// reaches no reader once the message loop has ended, so this read, made
/// after the last moment a map function can run and before the reason is
/// chosen, is the only one for a panic that landed after the loop. The
/// stop-time stage reports a panic of a map it runs itself.
fn collect_map_panic<A: Actor>(ctx: &mut ActorContext<A>, stop_reason: &mut StopReason) {
    if !ctx.has_end_state() {
        return;
    }
    let Some(panic) = ctx.stop_lane().abort_record().map_panic() else {
        return;
    };
    if matches!(stop_reason, StopReason::Kill | StopReason::Failure(_)) {
        return;
    }
    let err = ActorError::Panic(panic);
    ctx.record_failure(err.clone());
    *stop_reason = StopReason::Failure(err);
}

/// Applies a panic raised by a drop before the reason is final (a registered
/// timer's or stream's item at the forwarders' end, see
/// `ActorContext::end_forwarders`, a child factory or the supervision state
/// at the teardown): any reason but an earlier `Kill` or `Failure` becomes the
/// panic. A panic that leaves an earlier `Kill` or `Failure` as the reason is
/// logged once with the actor's id, as a drop after the seal is.
fn end_drop_panicked(
    stop_reason: &mut StopReason,
    payload: String,
    actor_id: &ActorId,
    record: &AbortRecord,
) {
    if !matches!(stop_reason, StopReason::Kill | StopReason::Failure(_)) {
        *stop_reason = StopReason::Failure(ActorError::Panic(payload));
        return;
    }
    #[cfg(feature = "tracing")]
    {
        let fields = crate::telemetry::log_fields(record, None);
        caught_log!(warn,
            actor_id = %actor_id,
            actor_name = fields.name(),
            incarnation = fields.incarnation(),
            system = fields.system(),
            reason = %fields.reason_text(stop_reason),
            panic = %fields.panic_text(&payload),
            "a drop before the actor's reason was final panicked; the reason stands"
        );
    }
    #[cfg(not(feature = "tracing"))]
    let _ = (payload, actor_id, record);
}

/// One event from either of the supervisor's two `JoinSet` planes, as
/// returned by [`next_supervision_event`].
enum SupervisionEvent {
    /// A restart attempt resolved (see the restart plane, `restart_set`). The
    /// outcome is boxed: it is large and rare, and the run loop holds the
    /// event across its awaits, so an outcome kept inline would be part of
    /// every actor's run future.
    Restart(Result<(tokio::task::Id, Box<RestartOutcome>), JoinError>),
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
    ctx.note_forwarders_polled();
    let (planes, mut forwarders) = ctx.supervision_planes_mut();
    let forwarders_pending = forwarders.as_ref().is_some_and(|f| !f.is_empty());
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
                    r.map(|joined| {
                        SupervisionEvent::Restart(
                            joined.map(|(task, outcome)| (task, Box::new(outcome))),
                        )
                    })
                }
                d = death_set.join_next_with_id(), if death_pending => {
                    d.map(SupervisionEvent::Death)
                }
                _ = verdicts.notified() => Some(SupervisionEvent::Verdict),
                f = join_forwarder(forwarders), if forwarders_pending => {
                    f.map(SupervisionEvent::Forwarder)
                }
            }
        }
        None => {
            if !forwarders_pending {
                return std::future::pending::<Option<SupervisionEvent>>().await;
            }
            join_forwarder(forwarders.take())
                .await
                .map(SupervisionEvent::Forwarder)
        }
    }
}

/// The next forwarder completion; pends forever with no forwarder plane.
async fn join_forwarder(
    forwarders: Option<&mut JoinSet<ForwarderEnd>>,
) -> Option<Result<(tokio::task::Id, ForwarderEnd), JoinError>> {
    match forwarders {
        Some(set) => set.join_next_with_id().await,
        None => std::future::pending().await,
    }
}

// ---------------------------------------------------------------------------
// Dispatch (user messages only - Stop travels on the stop lane)
// ---------------------------------------------------------------------------

/// A turn of a message and the chain of `continue_with` steps it asks for:
/// the form of the callers that hand a message over outside the run loop's
/// own turns (the receive timeout's message and the final deliveries).
/// `hot` is the run loop's cache of the handler list while the loop runs,
/// and `None` once it has ended, where every event of the turn goes through
/// the pump: the end that follows still owes its steps.
#[allow(clippy::too_many_arguments)]
async fn dispatch<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    envelope: ActorEnvelope<A>,
    turn_clock: &TurnClock,
    drain: &mut ReplyDrain,
    held: &mut HeldAnswer<A::Response>,
    mut hot: Option<&mut HotCache>,
) -> ControlFlow<StopReason> {
    let flow = dispatch_one(
        actor,
        ctx,
        envelope,
        turn_clock,
        drain,
        held,
        false,
        hot.as_deref_mut(),
    )
    .await;
    if !ctx.has_continuation() {
        return flow;
    }
    continue_chain(actor, ctx, flow, turn_clock, drain, held, hot).await
}

/// Runs the messages `continue_with` asked for after a callback, one after
/// the other, before the run loop reads any branch. `flow` is the outcome of
/// the turn that set the first one. The chain ends, and the message in the
/// cell is dropped, when a turn ends the actor, when the status reads
/// `Stopping`, and when a `Kill` is stored: the lane is read before each
/// step, so a `Kill` is not made to wait out the chain. Each step is a turn
/// of its own, with its turn events (see [`dispatch`] for `hot`).
async fn continue_chain<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    mut flow: ControlFlow<StopReason>,
    turn_clock: &TurnClock,
    drain: &mut ReplyDrain,
    held: &mut HeldAnswer<A::Response>,
    mut hot: Option<&mut HotCache>,
) -> ControlFlow<StopReason> {
    loop {
        // Each step is a message: it spends the task's cooperative budget, so
        // a chain of steps that never await still yields, and a `Kill` raised
        // by another task meanwhile is read below, before the next step.
        tokio::task::consume_budget().await;
        if flow.is_break() {
            ctx.drop_continuation();
            return flow;
        }
        if !ctx.has_continuation() {
            return flow;
        }
        if ctx.status() == ActorStatus::Stopping
            || matches!(ctx.stop_lane().reason(), Some(StopReason::Kill))
        {
            ctx.drop_continuation();
            return flow;
        }
        let Some(payload) = ctx.take_continuation() else {
            return flow;
        };
        let envelope = Envelope::Message {
            payload,
            responder: None,
        };
        flow = dispatch_one(
            actor,
            ctx,
            envelope,
            turn_clock,
            drain,
            held,
            false,
            hot.as_deref_mut(),
        )
        .await;
    }
}

/// Fires one threshold of the idle spell, inline on the actor's task: the
/// receive timeout's message goes through `dispatch`, the idle hook runs
/// under the turn clock as a hook turn of its own (`HookKind::OnIdle`, with
/// its caller token and its turn event). The threshold is marked fired
/// before it runs, and the turns it causes, the chain of `continue_with`
/// included, do not end the spell or clear the mark, so a zero length fires
/// once.
async fn fire_idle<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    kind: IdleKind,
    turn_clock: &TurnClock,
    drain: &mut ReplyDrain,
    held: &mut HeldAnswer<A::Response>,
    hot: Option<&mut HotCache>,
) -> ControlFlow<StopReason> {
    ctx.set_inline(true);
    ctx.mark_fired(kind);
    let flow = match kind {
        IdleKind::Receive => match ctx.make_receive_message() {
            None => ControlFlow::Continue(()),
            Some(Ok(payload)) => {
                let envelope = Envelope::Message {
                    payload,
                    responder: None,
                };
                dispatch(actor, ctx, envelope, turn_clock, drain, held, hot).await
            }
            Some(Err(panic)) => {
                let err = ActorError::Panic(panic);
                ctx.record_failure(err.clone());
                ControlFlow::Break(StopReason::Failure(err))
            }
        },
        IdleKind::Idle => {
            // A flag left by an earlier `stop` is not this hook's.
            let _ = ctx.take_self_stop();
            let outcome = {
                let turn = turn_clock.begin();
                let result =
                    catch_hook_t(turn_clock.caller_token(turn), || actor.on_idle(ctx)).await;
                if !turn.is_idle() {
                    let took = turn_clock.end_timed(turn);
                    push_turn(
                        ctx.stop_lane().abort_record(),
                        turn,
                        took,
                        HookKind::OnIdle,
                        outcome_of(&result),
                    );
                }
                result
            };
            match outcome {
                Ok(Ok(())) => {
                    if ctx.take_self_stop() {
                        ctx.drop_continuation();
                        ControlFlow::Continue(())
                    } else {
                        continue_chain(
                            actor,
                            ctx,
                            ControlFlow::Continue(()),
                            turn_clock,
                            drain,
                            held,
                            hot,
                        )
                        .await
                    }
                }
                Ok(Err(err)) => {
                    ctx.drop_continuation();
                    ctx.record_failure(err.clone());
                    ControlFlow::Break(StopReason::Failure(err))
                }
                Err(payload) => {
                    ctx.drop_continuation();
                    let err = ActorError::Panic(payload_into_string(payload));
                    ctx.record_failure(err.clone());
                    ControlFlow::Break(StopReason::Failure(err))
                }
            }
        }
    };
    ctx.set_inline(false);
    flow
}

/// What ended a wait on the mailbox and the idle sleep.
enum Wake<T> {
    /// The mailbox gave a message.
    Mail(T),
    /// The mailbox reported itself closed.
    Closed,
    /// The idle sleep for the nearest threshold ended.
    Idle,
}

/// Waits on the mailbox and then, behind it, on the idle sleep of an actor that
/// set a deadline. With no sleep it is the mailbox alone, so the select has no
/// branch of its own for the idle spell.
fn mail_or_idle<'a, T>(
    mailbox: &'a mut MailRx<T>,
    closed: bool,
    mut idle: Option<&'a mut Pin<Box<tokio::time::Sleep>>>,
) -> impl Future<Output = Wake<T>> + 'a {
    std::future::poll_fn(move |cx| {
        if !closed {
            if let Poll::Ready(received) = mailbox.poll_recv(cx) {
                return Poll::Ready(match received {
                    Some(envelope) => Wake::Mail(envelope),
                    None => Wake::Closed,
                });
            }
        }
        match idle.as_mut() {
            Some(sleep) => sleep.as_mut().poll(cx).map(|()| Wake::Idle),
            None => Poll::Pending,
        }
    })
}

/// How a plain turn ended, in one byte. The reason a turn ended the actor with
/// is written to the run loop's `stop_reason` by [`settle`], so nothing the
/// size of a `ControlFlow<StopReason>` is returned through memory on the
/// common turn.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum TurnEnd {
    /// The loop goes on.
    Next,
    /// The turn ended the actor; `stop_reason` holds why.
    Stop,
}

/// Turns the outcome of a cold path into a [`TurnEnd`], storing the reason of
/// a stop in `stop_reason`.
#[cold]
#[inline(never)]
fn settle(flow: ControlFlow<StopReason>, stop_reason: &mut StopReason) -> TurnEnd {
    match flow {
        ControlFlow::Continue(()) => TurnEnd::Next,
        ControlFlow::Break(reason) => {
            *stop_reason = reason;
            TurnEnd::Stop
        }
    }
}

/// The first step of a signal envelope: a monitor's notice reaches `handle` as
/// a message with no responder, unless the monitor was revoked with a flush
/// while the notice was queued: then its payload is dropped, a drop panic
/// being a panic of the turn, as an undelivered response's is. `Err` is the
/// outcome of a turn that runs no handler.
#[cold]
#[inline(never)]
fn open_signal<A: Actor>(
    ctx: &mut ActorContext<A>,
    tag: crate::types::SignalTag,
    payload: A::Message,
) -> Result<A::Message, ControlFlow<StopReason>> {
    if tag.shared.is_flushed() {
        return Err(match end_value(payload) {
            None => ControlFlow::Continue(()),
            Some(panic) => {
                let err = ActorError::Panic(panic);
                ctx.record_failure(err.clone());
                ControlFlow::Break(StopReason::Failure(err))
            }
        });
    }
    ctx.finish_signal(&tag);
    Ok(payload)
}

/// Sends the reply of a handler that returned `Ok`, or drops it when nobody
/// receives it. A response nobody receives (a notify, or a send whose caller
/// is gone) is dropped here, and its drop is user code: caught, its panic is
/// returned, and is a panic of the turn. `out` is set when the message had a
/// caller, to whether the reply reached it (for the debug state).
#[inline(always)]
fn deliver<A: Actor>(
    drain: &mut ReplyDrain,
    responder: &mut AnswerHold<Result<A::Response, ActorError>>,
    response: A::Response,
    out: &mut Option<bool>,
) -> Option<String> {
    let undelivered = match responder.take() {
        Some(tx) => {
            drain.replied();
            let sent = send_contained(tx, Ok(response)).err();
            *out = Some(sent.is_none());
            sent
        }
        None => Some(Ok(response)),
    };
    undelivered.and_then(end_value)
}

/// The turn failed because the drop of an undelivered response panicked.
#[cold]
#[inline(never)]
fn undelivered_panic<A: Actor>(
    ctx: &mut ActorContext<A>,
    payload: String,
) -> ControlFlow<StopReason> {
    ctx.drop_continuation();
    let err = ActorError::Panic(payload);
    ctx.record_failure(err.clone());
    ControlFlow::Break(StopReason::Failure(err))
}

/// The events of a `handle` turn that returned `Ok` and whose reply was
/// sent, raised on the run loop's own task: the debug state's trace entries
/// of the turn and `TurnFinished`. The loop owes only its next take here, so
/// the handlers run on this task from the loop's cached list. Kept out of the
/// turn: an actor with no debug option and no sink of `TurnFinished` never
/// calls it.
#[cold]
#[inline(never)]
fn turn_done_hot<A: Actor>(
    ctx: &mut ActorContext<A>,
    hot: &mut HotCache,
    turn: TurnMode,
    took: Option<Duration>,
    out: Option<bool>,
) {
    if ctx.debug_on() {
        for entry in debug_after_turn(ctx, out).into_iter().flatten() {
            crate::telemetry::emit_hot(
                ctx.stop_lane().abort_record(),
                None,
                hot,
                EventKind::Trace,
                |actor| TelemetryEvent::Trace {
                    actor: Cow::Borrowed(actor),
                    entry,
                },
            );
        }
    }
    if turn.armed(EventKind::TurnFinished) {
        crate::telemetry::emit_hot(
            ctx.stop_lane().abort_record(),
            None,
            hot,
            EventKind::TurnFinished,
            |actor| TelemetryEvent::TurnFinished {
                actor: Cow::Borrowed(actor),
                hook: HookKind::Handle,
                took: took.unwrap_or_default(),
                outcome: TurnOutcome::Returned,
            },
        );
    }
}

/// The same events for a turn whose path still owes the crate a step: a
/// turn after the run loop ended (a final delivery), or a turn whose handler
/// stopped its own actor. They are queued on the pump, which runs the
/// handlers on its helper; nothing runs a handler on this task.
#[cold]
#[inline(never)]
fn turn_done_pumped<A: Actor>(
    ctx: &mut ActorContext<A>,
    turn: TurnMode,
    took: Option<Duration>,
    out: Option<bool>,
) {
    if ctx.debug_on() {
        for entry in debug_after_turn(ctx, out).into_iter().flatten() {
            push_about(
                ctx.stop_lane().abort_record(),
                None,
                EventKind::Trace,
                |actor| TelemetryEvent::Trace {
                    actor: Cow::Owned(actor.clone()),
                    entry,
                },
            );
        }
    }
    push_turn(
        ctx.stop_lane().abort_record(),
        turn,
        took,
        HookKind::Handle,
        TurnOutcome::Returned,
    );
}

/// Everything a turn does after its handler returned, but the common case: a
/// handler that returned `Ok` and did not stop its own actor is finished in
/// the run loop. The request's sender is held in a guard for the turn: it is
/// answered, or handed to `held` for a failed request, and if this task
/// unwinds first, its caller's waker is woken inside a catch and adds no
/// second panic to the unwind. `turn` and `took` are the turn's mode and
/// length, for its `TurnFinished`; `hot` is the run loop's cache while the
/// loop runs (see [`dispatch`]).
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn finish_turn<A: Actor>(
    ctx: &mut ActorContext<A>,
    mut responder: AnswerHold<Result<A::Response, ActorError>>,
    outcome: Result<Result<A::Response, ActorError>, crate::actor::panic::PanicPayload>,
    drain: &mut ReplyDrain,
    held: &mut HeldAnswer<A::Response>,
    turn: TurnMode,
    took: Option<Duration>,
    hot: Option<&mut HotCache>,
) -> ControlFlow<StopReason> {
    match outcome {
        Ok(Ok(response)) => {
            // The handler stopped its own actor and returned a reply:
            // the reply is held to the seal of the stop lane, as a
            // failed request's answer is (see `HeldAnswer`). The lane
            // is the top branch of the loop, and a raise leaves an
            // unseen version, so nothing queued is handled before
            // the stop is read.
            let response = if ctx.take_self_stop() {
                ctx.drop_continuation();
                match responder.take() {
                    Some(tx) => {
                        log_discarded_answer(
                            held.hold(
                                tx,
                                Ok(response),
                                ctx.actor_id(),
                                ctx.stop_lane().abort_record(),
                            ),
                            ctx.actor_id(),
                            ctx.stop_lane().abort_record(),
                        );
                        turn_done_pumped(ctx, turn, took, None);
                        return ControlFlow::Continue(());
                    }
                    None => {
                        let mut out = None;
                        return match deliver::<A>(drain, &mut responder, response, &mut out) {
                            None => {
                                turn_done_pumped(ctx, turn, took, out);
                                ControlFlow::Continue(())
                            }
                            Some(payload) => {
                                push_turn(
                                    ctx.stop_lane().abort_record(),
                                    turn,
                                    took,
                                    HookKind::Handle,
                                    TurnOutcome::Panicked,
                                );
                                undelivered_panic(ctx, payload)
                            }
                        };
                    }
                }
            } else {
                response
            };
            let mut out = None;
            match deliver::<A>(drain, &mut responder, response, &mut out) {
                None => {
                    match hot {
                        Some(hot) => {
                            if ctx.debug_on() || turn.armed(EventKind::TurnFinished) {
                                turn_done_hot(ctx, hot, turn, took, out);
                            }
                        }
                        None => turn_done_pumped(ctx, turn, took, out),
                    }
                    ControlFlow::Continue(())
                }
                Some(payload) => {
                    push_turn(
                        ctx.stop_lane().abort_record(),
                        turn,
                        took,
                        HookKind::Handle,
                        TurnOutcome::Panicked,
                    );
                    undelivered_panic(ctx, payload)
                }
            }
        }
        Ok(Err(err)) => {
            push_turn(
                ctx.stop_lane().abort_record(),
                turn,
                took,
                HookKind::Handle,
                TurnOutcome::Failed,
            );
            // Cast-exception parity: an `Err` from `handle` stops
            // the actor identically on the notify and send paths. The
            // send path additionally has a caller to notify, and its
            // answer is held until the actor's name is free and its
            // lane is sealed (see `HeldAnswer`).
            if let Some(tx) = responder.take() {
                log_discarded_answer(
                    held.hold(
                        tx,
                        Err(err.clone()),
                        ctx.actor_id(),
                        ctx.stop_lane().abort_record(),
                    ),
                    ctx.actor_id(),
                    ctx.stop_lane().abort_record(),
                );
            }
            ctx.drop_continuation();
            ctx.record_failure(err.clone());
            ControlFlow::Break(StopReason::Failure(err))
        }
        Err(payload) => {
            push_turn(
                ctx.stop_lane().abort_record(),
                turn,
                took,
                HookKind::Handle,
                TurnOutcome::Panicked,
            );
            // A panic crashes the actor on BOTH paths exactly like a
            // returned `Err` does (cast-exception parity, same as the
            // arm above): the only difference is the panic payload
            // has to be caught and wrapped here instead of being
            // handed back verbatim. A send() caller still receives it
            // as a matchable error, once the actor has ended.
            let err = ActorError::Panic(payload_into_string(payload));
            if let Some(tx) = responder.take() {
                log_discarded_answer(
                    held.hold(
                        tx,
                        Err(err.clone()),
                        ctx.actor_id(),
                        ctx.stop_lane().abort_record(),
                    ),
                    ctx.actor_id(),
                    ctx.stop_lane().abort_record(),
                );
            }
            ctx.drop_continuation();
            ctx.record_failure(err.clone());
            ControlFlow::Break(StopReason::Failure(err))
        }
    }
}

/// The general turn, for the callers that are not the run loop's plain path
/// (the priority lane, the continuation chain, the idle fire and the final
/// deliveries). It follows the same steps as the plain turn in the run loop,
/// its turn events included. `priority` says which lane the message came
/// from, for the debug state; `hot` is the run loop's cache while the loop
/// runs (see [`dispatch`]).
#[allow(clippy::too_many_arguments)]
async fn dispatch_one<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    envelope: ActorEnvelope<A>,
    turn_clock: &TurnClock,
    drain: &mut ReplyDrain,
    held: &mut HeldAnswer<A::Response>,
    priority: bool,
    hot: Option<&mut HotCache>,
) -> ControlFlow<StopReason> {
    let (payload, responder) = match envelope {
        Envelope::Signal { tag, payload } => match open_signal(ctx, tag, payload) {
            Ok(payload) => (payload, None),
            Err(flow) => return flow,
        },
        Envelope::Message { payload, responder } => (payload, responder),
    };
    let responder = AnswerHold::new(responder);
    // `begin` and `end` bracket the callback exactly, on every
    // outcome (normal return, an `Err`, or a caught panic), since
    // `catch_hook_t` always completes normally here rather than
    // unwinding through this function.
    ctx.clear_self_stop();
    let turn = turn_clock.begin();
    // The debug state, when the actor has any option on: the message is
    // counted and logged inside the turn, so an installed function that
    // blocks is a turn the watchdog sees. An actor with none pays this one
    // test.
    if let Some(state) = ctx.debug_mut() {
        state.message_in(responder.is_some(), priority);
    }
    // One poll site, as in the run loop's plain turn: the caller token is
    // `0`, and sets nothing, for a turn that records no wait.
    let outcome = catch_hook_t(turn_clock.caller_token(turn), || actor.handle(payload, ctx)).await;
    let took = if turn.is_idle() {
        None
    } else {
        turn_clock.end_timed(turn)
    };
    #[cfg(test)]
    end_hooks::run(end_hooks::TURN, ctx.actor_id());
    finish_turn(ctx, responder, outcome, drain, held, turn, took, hot)
}

/// The outcome of a plain turn as the general form, for the chain of
/// `continue_with` steps: a stop moves its reason out of `stop_reason`, which
/// [`settle`] writes back when the chain ends.
#[cold]
#[inline(never)]
fn flow_of(end: TurnEnd, stop_reason: &mut StopReason) -> ControlFlow<StopReason> {
    match end {
        TurnEnd::Next => ControlFlow::Continue(()),
        TurnEnd::Stop => ControlFlow::Break(std::mem::replace(stop_reason, StopReason::Graceful)),
    }
}

/// Runs the messages `continue_with` asked for after a turn of the run loop's
/// plain path. Kept out of line: a turn that set none never gets here.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
async fn chain_after<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    flow: ControlFlow<StopReason>,
    turn_clock: &TurnClock,
    drain: &mut ReplyDrain,
    held: &mut HeldAnswer<A::Response>,
    gave_back: &mut bool,
    hot: &mut HotCache,
) -> ControlFlow<StopReason> {
    watch_yield(
        || continue_chain(actor, ctx, flow, turn_clock, drain, held, Some(hot)),
        gave_back,
    )
    .await
}

/// Logs, with the `tracing` feature, the panic from dropping an earlier held
/// answer that a newer one replaced. The newer answer decides the turn, so the
/// panic is not the actor's reason.
fn log_discarded_answer(panic: Option<String>, owner: &ActorId, record: &AbortRecord) {
    #[cfg(feature = "tracing")]
    if let Some(panic) = &panic {
        caught_log!(warn,
            actor_id = %owner,
            panic = %crate::telemetry::log_fields(record, None).panic_text(panic),
            "dropping an answer a newer one replaced panicked; the panic is contained"
        );
    }
    #[cfg(not(feature = "tracing"))]
    let _ = (panic, owner, record);
}

/// What the debug state does once a turn returned and its reply is sent: the
/// reply is counted and logged when the message had a caller (`out` carries
/// whether it was delivered), and the entries of the turn come back for the
/// trace to raise, none while the trace is off. Kept out of the turn's future,
/// which an actor with no debug option never enters.
#[inline(never)]
fn debug_after_turn<A: Actor>(
    ctx: &mut ActorContext<A>,
    out: Option<bool>,
) -> [Option<DebugEntry>; 2] {
    let Some(state) = ctx.debug_mut() else {
        return [None, None];
    };
    if let Some(delivered) = out {
        state.message_out(delivered);
    }
    if state.is_tracing() {
        state.take_traced()
    } else {
        [None, None]
    }
}

/// The answer to the one request whose handler failed (an `Err` or a panic),
/// or whose handler stopped its own actor and replied, held by the run loop
/// to the seal of its stop lane.
///
/// A caller reads a crash the way an Erlang/OTP `gen_server:call` caller does,
/// through its monitor, after the process is dead: the answer goes out when
/// the actor's reason is final, with its name free and its lane sealed, so a
/// caller that looks the actor up by name on reading it finds no registration
/// or the next incarnation, never this one. Until then the request is
/// unanswered, like a request that is still queued. A `Kill` that ends the
/// actor first, and a task that ends without reaching the seal (an abort, the
/// end of its runtime, its own unwind), drop the sender unanswered, and the
/// caller reads the final reason from the status plane. The sender is held in
/// an [`AnswerHold`], so the drop that wakes the caller never panics into an
/// unwind of the task, and the answer's value is dropped inside a catch on
/// every path, the task's end before the seal included, a panic of that drop
/// logged with the actor's id. The slot is boxed, so an actor that never fails
/// a request holds one pointer.
struct HeldAnswer<R> {
    slot: Option<Box<HeldReply<R>>>,
}

/// A held request's reply channel and the answer its caller is answered with:
/// the handler's error, or the reply of a handler that stopped its actor; with
/// the actor's id and record, which name the actor in the log line of a panic
/// from dropping the answer.
struct HeldReply<R> {
    sender: AnswerHold<Result<R, ActorError>>,
    answer: Result<R, ActorError>,
    owner: ActorId,
    record: AbortRecord,
}

impl<R> HeldAnswer<R> {
    const fn new() -> Self {
        Self { slot: None }
    }

    /// Keeps the answer for the seal. A failed request ends the loop at that
    /// request, and a self-stop is read by the loop before the next message
    /// is taken, so one answer is held at a time; an earlier one is sent
    /// first, so no caller is left unanswered. Returns the payload of a panic
    /// from dropping that earlier answer, which the caller logs: the answer
    /// that is held now decides the turn.
    fn hold(
        &mut self,
        tx: oneshot::Sender<Result<R, ActorError>>,
        answer: Result<R, ActorError>,
        owner: &ActorId,
        record: &AbortRecord,
    ) -> Option<String> {
        let discarded = self.release_now();
        self.slot = Some(Box::new(HeldReply {
            sender: AnswerHold::new(Some(tx)),
            answer,
            owner: owner.clone(),
            record: record.clone(),
        }));
        discarded
    }

    /// Sends the held answer at once, whatever the actor's end: the stop that
    /// held it was vetoed, and the actor keeps running. Returns the payload of
    /// a panic from dropping an answer nobody received.
    fn release_now(&mut self) -> Option<String> {
        let HeldReply {
            mut sender, answer, ..
        } = *self.slot.take()?;
        let Some(tx) = sender.take() else {
            return end_value(answer);
        };
        send_contained(tx, answer).err().and_then(end_value)
    }

    /// Sends the held answer unless a `Kill` ended the actor: its caller then
    /// reads the final reason, `Kill`, from the status plane. Returns the
    /// payload of a panic from dropping an answer nobody received.
    fn deliver(&mut self, final_reason: &StopReason) -> Option<String> {
        if !matches!(final_reason, StopReason::Kill) {
            return self.release_now();
        }
        let HeldReply {
            mut sender, answer, ..
        } = *self.slot.take()?;
        if let Some(tx) = sender.take() {
            // Dropping the sender wakes the caller, who then reads `Kill`; a
            // panic from that caller's waker stays out of this task.
            contain_waker_panic("reply", None, move || drop(tx));
        }
        end_value(answer)
    }
}

/// A task that ends before the seal (an abort, the end of its runtime, its
/// own unwind) drops the held answer here: the sender unanswered, inside the
/// catch of its [`AnswerHold`], and the value inside a catch, so a panic of
/// its drop neither leaves the task's drop nor adds a second panic to an
/// unwind.
impl<R> Drop for HeldAnswer<R> {
    fn drop(&mut self) {
        let Some(held) = self.slot.take() else {
            return;
        };
        let HeldReply {
            sender,
            answer,
            owner,
            record,
        } = *held;
        drop(sender);
        log_unsealed_answer(end_value(answer), &owner, &record);
    }
}

/// Logs, with the `tracing` feature, a panic from dropping a held answer at a
/// task end before the seal.
fn log_unsealed_answer(panic: Option<String>, owner: &ActorId, record: &AbortRecord) {
    #[cfg(feature = "tracing")]
    if let Some(panic) = &panic {
        caught_log!(warn,
            actor_id = %owner,
            panic = %crate::telemetry::log_fields(record, None).panic_text(panic),
            "dropping a held answer at the end of the actor's task panicked; the panic is contained"
        );
    }
    #[cfg(not(feature = "tracing"))]
    let _ = (panic, owner, record);
}

/// How many turns the run loop may run, once a reply is known to stand alone,
/// before it hands the thread back once.
const REPLY_DRAIN_TURNS: u32 = 3;

/// How many turns after a status reply the run loop does not take the turn
/// boundary fast path: for these turns it runs the select, which reads every
/// branch in order, and then returns to the fast path. Only a reply to a
/// requester outside every runtime task opens the window (a thread of its
/// own, or a body driven by `block_on`, a `#[tokio::main]` or a
/// `#[tokio::test]`), and a reply inside the window restarts the count. Such
/// a client reaches the actor while it drains a backlog; the window makes the
/// actor read the system channel and every other branch in order for these
/// turns instead of taking the mailbox directly. A reply to a requester that
/// is a runtime task opens no window: its reply is marked for the hand-back
/// instead (see `ReplyDrain`). Each request carries its requester's kind, so
/// the kind a reply reads is that of the request it answers.
const STATUS_FAST_PATH_OFF_TURNS: u32 = 32;

/// Answers one status request and records what the reply owes the loop: the
/// fast path window follows a reply to a requester outside every runtime
/// task, and the hand-back follows a delivered reply to a runtime task, or to
/// any requester on a current-thread runtime. A requester that is gone is
/// owed no hand-back.
fn answer_status(
    request: crate::types::StatusRequest,
    info: ActorStatusInfo,
    same_thread: bool,
    fp_skip: &mut u32,
    drain: &mut ReplyDrain,
) {
    let crate::types::StatusRequest { reply, task } = request;
    let delivered = send_contained(reply, Box::new(info)).is_ok();
    if !task {
        *fp_skip = STATUS_FAST_PATH_OFF_TURNS;
    }
    if delivered && (task || same_thread) {
        drain.replied();
    }
}

/// The longest a run of fast-path turns holds the worker before the loop
/// gives it up (a requeue, or the armed deferral on one worker), so a worker
/// shared with hot actors reaches its periodic driver poll (timers, I/O)
/// within the runtime's maintenance interval of such turns even when
/// handlers are slow. A run is measured from its first take and starts over
/// whenever the worker was given up.
const TURN_QUANTUM: std::time::Duration = std::time::Duration::from_micros(100);

/// Waits like `hand_back` but through tokio's deferral (`yield_now`): the
/// worker runs the rest of its queue, and parks and polls its drivers when
/// it has nothing else, before the task runs again. The wake slot is armed,
/// so a stop request or a status query that lands meanwhile wakes it at once.
async fn defer_armed(slot: &crate::types::WakeSlot, mut idle: impl FnMut() -> bool) {
    let mut yielded = std::pin::pin!(tokio::task::yield_now());
    std::future::poll_fn(|cx| {
        slot.register(cx.waker());
        if !idle() {
            return Poll::Ready(());
        }
        yielded.as_mut().poll(cx)
    })
    .await;
    slot.disarm();
}

/// Ends a turn with the task queued again at the back of its worker's run
/// queue: the task wakes itself and returns pending once, so the tasks
/// already runnable on the worker, the senders the takes woke among them,
/// run before it, and the worker does not wait for an idle moment or poll
/// its drivers first, as a `tokio::task::yield_now` would.
async fn requeue() {
    let mut queued = false;
    std::future::poll_fn(|cx| {
        if queued {
            return Poll::Ready(());
        }
        queued = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
}

/// Polls a turn's future and records whether it returned pending, that is,
/// whether the task gave its worker back while the turn ran.
async fn watch_yield<F: Future>(start: impl FnOnce() -> F, yielded: &mut bool) -> F::Output {
    let mut turn = std::pin::pin!(start());
    std::future::poll_fn(|cx| {
        let polled = turn.as_mut().poll(cx);
        if polled.is_pending() {
            *yielded = true;
        }
        polled
    })
    .await
}

/// Starts a turn: whether the fast path is off for it, and counts the turn
/// off the window a status reply opened.
#[inline(always)]
fn fp_skip_step(skip: &mut u32) -> bool {
    let off = *skip > 0;
    if off {
        *skip -= 1;
    }
    off
}

/// What the run loop remembers about the requesters it answered.
///
/// Which requesters a status reply marks: a requester that is a runtime task
/// is marked on every runtime: on the actor's own runtime the reply queues it
/// on the actor's worker, and a task of another runtime costs at most one
/// needless yield per lone reply; a requester that is not a task (a body
/// driven by `block_on`, a `#[tokio::main]` or `#[tokio::test]` body) is
/// marked only on a current-thread runtime, where the body runs on the
/// actor's own thread and waits for the poll to end. On a multi-thread runtime such a requester is
/// woken from another thread and does not wait for this poll, so its reply is
/// not marked. A reply to a call is always marked. A reply whose requester is
/// gone is marked for nobody: nothing waits for it.
///
/// A requester woken by a reply sent from this task is queued on this
/// worker, and runs only when this task's poll ends; with a backlog that is
/// the task's whole cooperative budget, so a status query or a call would
/// wait behind every message the poll still drains. When a reply stands
/// alone, with no reply on the turn after it, the loop therefore runs
/// `REPLY_DRAIN_TURNS` more turns and then hands the thread back once at a
/// turn boundary, if a lane still holds messages. Replies on consecutive
/// turns come from many requesters, each woken in turn and queued ahead of
/// the last, so no one is left behind the whole backlog and the loop keeps
/// draining. A loop with nothing queued at a boundary is about to wait
/// anyway, so the state is dropped there. Nothing here moves a signal or
/// changes which branch is read first: the hand-back is a suspension
/// between two turns, after which the loop reads every branch in order.
///
/// One word, zero while no reply is pending, so the turns that answer
/// nobody read it and change nothing: the turns left before the hand-back
/// while one is due, otherwise `PENDING` (a reply was seen and the turn
/// after it is awaited) with `REPLIED` set when the turn that just ran
/// replied.
struct ReplyDrain(u32);

impl ReplyDrain {
    /// The turn that just ran sent a reply.
    const REPLIED: u32 = 1 << 31;
    /// A reply was seen; the turn after it decides whether it stood alone.
    const PENDING: u32 = 1 << 30;
    /// The turns left before the hand-back.
    const LEFT: u32 = Self::PENDING - 1;

    const fn new() -> Self {
        Self(0)
    }

    /// A reply was sent.
    fn replied(&mut self) {
        self.0 |= Self::REPLIED;
    }

    /// Advances the state at a turn boundary with a backlog; whether the
    /// loop should hand the thread back now.
    fn step(&mut self) -> bool {
        let s = self.0;
        if s & Self::LEFT != 0 {
            self.0 = (s & Self::LEFT) - 1;
            self.0 == 0
        } else if s & Self::REPLIED != 0 {
            self.0 = Self::PENDING;
            false
        } else {
            self.0 = REPLY_DRAIN_TURNS;
            false
        }
    }
}

/// Hands the thread back for one scheduler round, the way a poll whose
/// cooperative budget ran out does: the task is queued again behind the
/// tasks already runnable on this worker, and a requester just woken runs
/// first. While handed back the loop waits like a loop in its select: the
/// wake slot is armed, so a stop request or a status query that lands now
/// wakes it at once instead of waiting for the worker's next idle moment,
/// and `idle` is read after the arming, so one that landed before it ends
/// the hand-back immediately.
async fn hand_back(slot: &crate::types::WakeSlot, mut idle: impl FnMut() -> bool) {
    let mut yielded = std::pin::pin!(requeue());
    std::future::poll_fn(|cx| {
        slot.register(cx.waker());
        if !idle() {
            return Poll::Ready(());
        }
        yielded.as_mut().poll(cx)
    })
    .await;
    slot.disarm();
}

/// The run loop's select branches above the mailbox, one variant per branch.
/// The fast path reads each one's clause in `branch_idle`, whose match is
/// exhaustive, and every supervision event maps to one of them
/// (`SupervisionEvent::branch`), so a new plane or branch does not compile
/// until its clause is decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpperBranch {
    Stop,
    System,
    Restart,
    Death,
    Verdict,
    Forwarder,
    Priority,
}

impl UpperBranch {
    /// The select's branch order, highest first; the mailbox follows.
    const ORDER: [UpperBranch; 7] = [
        UpperBranch::Stop,
        UpperBranch::System,
        UpperBranch::Restart,
        UpperBranch::Death,
        UpperBranch::Verdict,
        UpperBranch::Forwarder,
        UpperBranch::Priority,
    ];
}

impl SupervisionEvent {
    /// The select branch this event arrives on.
    #[cfg_attr(not(test), allow(dead_code))]
    fn branch(&self) -> UpperBranch {
        match self {
            SupervisionEvent::Restart(_) => UpperBranch::Restart,
            SupervisionEvent::Death(_) => UpperBranch::Death,
            SupervisionEvent::Verdict => UpperBranch::Verdict,
            SupervisionEvent::Forwarder(_) => UpperBranch::Forwarder,
        }
    }
}

/// The branches that end a reply's hand-back: a stop request and a system
/// message must not wait behind it. The other planes hold standing state
/// (a supervisor's running children, its timers, a priority backlog), which
/// reads busy while it is merely non-empty, so they do not decide whether the
/// requester a reply woke gets its turn.
const HAND_BACK_EXIT: [UpperBranch; 2] = [UpperBranch::Stop, UpperBranch::System];

/// Whether none of the [`HAND_BACK_EXIT`] branches has anything for the run
/// loop now.
fn hand_back_idle<A: Actor>(
    stop_rx: &LaneRx,
    system_rx: &SystemPlane,
    ctx: &mut ActorContext<A>,
    priority: &Option<mpsc::Receiver<ActorEnvelope<A>>>,
) -> bool {
    HAND_BACK_EXIT
        .into_iter()
        .all(|branch| branch_idle(branch, stop_rx, system_rx, ctx, priority))
}

/// Whether every select branch above the mailbox reads idle now, read in the
/// select's branch order.
fn upper_idle<A: Actor>(
    stop_rx: &LaneRx,
    system_rx: &SystemPlane,
    ctx: &mut ActorContext<A>,
    priority: &Option<mpsc::Receiver<ActorEnvelope<A>>>,
) -> bool {
    UpperBranch::ORDER
        .into_iter()
        .all(|branch| branch_idle(branch, stop_rx, system_rx, ctx, priority))
}

/// One branch's clause: idle exactly when the select's poll of that branch
/// would pend.
fn branch_idle<A: Actor>(
    branch: UpperBranch,
    stop_rx: &LaneRx,
    system_rx: &SystemPlane,
    ctx: &mut ActorContext<A>,
    priority: &Option<mpsc::Receiver<ActorEnvelope<A>>>,
) -> bool {
    match branch {
        // No unseen version.
        UpperBranch::Stop => matches!(stop_rx.has_changed(), Ok(false)),
        // Not opened, or opened, taken by the run loop, empty and open. A
        // plane opened and not yet taken reads busy, so the select's branch
        // takes its receiver and answers.
        UpperBranch::System => system_rx.is_idle(),
        UpperBranch::Restart => {
            let (planes, _) = ctx.supervision_planes_mut();
            planes.is_none_or(|(restart, _, _)| restart.is_empty())
        }
        UpperBranch::Death => {
            let (planes, _) = ctx.supervision_planes_mut();
            planes.is_none_or(|(_, death, _)| death.is_empty())
        }
        // A supervisor's branch polls its verdict signal whenever any plane
        // holds a task, the forwarder plane included, so it reads idle only
        // with every plane empty.
        UpperBranch::Verdict => {
            let (planes, forwarders) = ctx.supervision_planes_mut();
            planes.is_none_or(|(restart, death, _)| {
                restart.is_empty() && death.is_empty() && forwarders.is_none_or(|f| f.is_empty())
            })
        }
        // A supervisor reads its forwarder plane by emptiness, as the
        // verdict clause does; any other actor by the forwarder end count,
        // without the plane's lock.
        UpperBranch::Forwarder => {
            if ctx.supervision_ref().is_some() {
                let (_, forwarders) = ctx.supervision_planes_mut();
                forwarders.is_none_or(|f| f.is_empty())
            } else {
                ctx.forwarders_idle()
            }
        }
        UpperBranch::Priority => priority
            .as_ref()
            .is_none_or(|lane| lane.is_empty() && !lane.is_closed()),
    }
}

/// The stop lane's next unseen version, woken through the run loop's
/// single-reader slot: armed before the second read, so a raise that lands
/// between the reads is seen by it, and one that lands after wakes the loop.
/// The slot is disarmed when this future drops (the select dropping its
/// branches), so no raise wakes the loop during a callback. The version is
/// marked seen by the branch's own `borrow_and_update`.
fn lane_changed<'a>(rx: &'a LaneRx, slot: &'a crate::types::WakeSlot) -> LaneChanged<'a> {
    LaneChanged { rx, slot }
}

struct LaneChanged<'a> {
    rx: &'a LaneRx,
    slot: &'a crate::types::WakeSlot,
}

impl LaneChanged<'_> {
    fn unseen(&self) -> bool {
        matches!(self.rx.has_changed(), Ok(true))
    }
}

impl Future for LaneChanged<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if self.unseen() {
            return Poll::Ready(());
        }
        self.slot.register(cx.waker());
        if self.unseen() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for LaneChanged<'_> {
    fn drop(&mut self) {
        self.slot.disarm();
    }
}

/// The mailbox's next message if one is ready now, through the task's own
/// context (so the cooperative budget applies); `None` when it is empty,
/// closed or not ready, which the select then decides. Never waits.
fn poll_now<T>(rx: &mut MailRx<T>) -> impl Future<Output = Option<T>> + '_ {
    std::future::poll_fn(move |cx| match rx.poll_recv(cx) {
        Poll::Ready(Some(value)) => Poll::Ready(Some(value)),
        Poll::Ready(None) | Poll::Pending => Poll::Ready(None),
    })
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
            child.transition_expected(ChildLifecycle::Down {
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

/// Queues the death event of the child incarnation `incarnation`, with the
/// decision the event carries: the supervisor's own identity names the
/// supervisor (its id read as a root with incarnation 0 when it was spawned
/// with none), and the child's reference is rebuilt from it. Called before
/// the callback, with the decision already made, so the budget event of the
/// same death comes first.
fn emit_child_died<A: Actor>(ctx: &ActorContext<A>, event: &ChildEvent, incarnation: u64) {
    crate::telemetry::with_mode(
        ctx.stop_lane().abort_record(),
        ctx.system_in_hand(),
        |mode| {
            if !mode.armed(EventKind::ChildDied) {
                return;
            }
            let (supervisor, owner) = crate::telemetry::supervisor_of(
                mode,
                ctx.stop_lane().abort_record(),
                ctx.actor_id(),
            );
            crate::telemetry::push_for(mode, owner.as_ref(), EventKind::ChildDied, || {
                TelemetryEvent::ChildDied {
                    child: ActorRef::for_child_of(&supervisor, event.child_id.clone(), incarnation),
                    supervisor,
                    reason: event.reason.clone(),
                    decision: event.action,
                }
            });
        },
    );
}

/// Queues the budget event of one charge against the restart budget of the
/// supervisor behind `ctx`: the death of `child` (the incarnation
/// `incarnation`) was charged, and the charge stands as `charge` says.
fn emit_restart_budget<A: Actor>(
    ctx: &ActorContext<A>,
    child: &ActorId,
    incarnation: u64,
    charge: &BudgetCharge,
) {
    // The charge that exhausts the budget is the one the supervisor does not
    // accept: it stops there, so this is the only report of it.
    if !charge.accepted {
        reports::reached_max_restart_intensity(
            ctx.actor_id(),
            child,
            incarnation,
            charge.max,
            charge.window,
        );
    }
    crate::telemetry::with_mode(
        ctx.stop_lane().abort_record(),
        ctx.system_in_hand(),
        |mode| {
            if !mode.armed(EventKind::RestartBudget) {
                return;
            }
            let (supervisor, owner) = crate::telemetry::supervisor_of(
                mode,
                ctx.stop_lane().abort_record(),
                ctx.actor_id(),
            );
            crate::telemetry::push_for(mode, owner.as_ref(), EventKind::RestartBudget, || {
                TelemetryEvent::RestartBudget {
                    child: ActorRef::for_child_of(&supervisor, child.clone(), incarnation),
                    supervisor,
                    used: charge.used,
                    max: charge.max,
                    window: charge.window,
                    accepted: charge.accepted,
                }
            });
        },
    );
}

/// Delivers one `on_child_stopped`; a failing callback stops the supervisor.
async fn deliver_child_event<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    event: &ChildEvent,
    incarnation: u64,
) -> ControlFlow<StopReason> {
    emit_child_died(ctx, event, incarnation);
    let clock = ctx.turn_clock();
    let turn = clock.begin();
    let delivered = catch_hook_t(clock.caller_token(turn), || {
        actor.on_child_stopped(event, ctx)
    })
    .await;
    let took = clock.end_timed(turn);
    push_turn(
        ctx.stop_lane().abort_record(),
        turn,
        took,
        HookKind::OnChildStopped,
        outcome_of(&delivered),
    );
    // The chain does not run after this callback: a message it asked for is
    // dropped here, not carried to a later event.
    ctx.drop_continuation();
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
    handle_child_stopped_with(actor, ctx, event, true).await
}

/// [`handle_child_stopped`]. `terminated` says whether the supervisor's
/// report `child_terminated` covers the first report: false for the end of a
/// restart attempt that never started an incarnation, which is reported as a
/// `start_error` where the attempt's outcome is handled.
async fn handle_child_stopped_with<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    event: ChildStoppedInternal,
    terminated: bool,
) -> ControlFlow<StopReason> {
    // The report in hand is held apart from the worklist, so the worklist
    // allocates only when a second report queues behind it.
    let mut work: VecDeque<ChildStoppedInternal> = VecDeque::new();
    let flow = handle_worklist_with(actor, ctx, Some(event), &mut work, terminated).await;
    if flow.is_break() {
        // The loop ends with reports still in the worklist: they are
        // collected for their own deliveries.
        if let Some(sup) = ctx.supervision_mut() {
            sup.owed.extend(work);
        }
    }
    flow
}

/// The debt of one reported death: owed to the supervision state's decision
/// until the pass over it ends, which clears it on every path out, an early
/// return or an unwind included, unless the pass keeps it.
struct Decision {
    owner: Option<AbortRecord>,
    incarnation: u64,
    kept: bool,
}

impl Decision {
    fn new(owner: Option<AbortRecord>, incarnation: u64) -> Self {
        Self {
            owner,
            incarnation,
            kept: false,
        }
    }

    /// The report is parked or goes through the worklist again: the debt
    /// stays until the pass that decides it.
    fn keep(&mut self) {
        self.kept = true;
    }
}

impl Drop for Decision {
    fn drop(&mut self) {
        if !self.kept {
            if let Some(owner) = &self.owner {
                crate::telemetry::death_decided(owner, self.incarnation);
            }
        }
    }
}

/// Runs the worklist of [`handle_child_stopped`] until it is empty: `first`
/// is handled before anything in `work`, and a report pushed back to the
/// front of `work` is handled next.
async fn handle_worklist<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    first: Option<ChildStoppedInternal>,
    work: &mut VecDeque<ChildStoppedInternal>,
) -> ControlFlow<StopReason> {
    handle_worklist_with(actor, ctx, first, work, true).await
}

/// [`handle_worklist`]; `first_terminated` says whether the supervisor's
/// report `child_terminated` covers `first`.
async fn handle_worklist_with<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    mut first: Option<ChildStoppedInternal>,
    work: &mut VecDeque<ChildStoppedInternal>,
    first_terminated: bool,
) -> ControlFlow<StopReason> {
    let mut terminated = first_terminated;
    while let Some(ev) = first.take().or_else(|| work.pop_front()) {
        // Only the first report of the call can be an attempt's: the rest
        // came from the worklist and are deaths of incarnations.
        let reports_death = std::mem::replace(&mut terminated, true);
        // The death in hand is owed a decision until this pass over it ends
        // (the restart it started has its ticket by then), whichever way it
        // ends; a pass that parks or re-queues the report keeps the debt.
        let mut decision = Decision::new(
            ctx.supervision_ref()
                .and_then(|sup| sup.registry.owner_record()),
            ev.incarnation,
        );
        // The end of a child whose awaited start failed: it kept no spec, so
        // its report changes nothing and reaches no callback.
        if ctx.supervision_mut().is_some_and(|sup| {
            !sup.silenced.is_empty() && sup.take_silenced(&ev.child_id, ev.incarnation)
        }) {
            continue;
        }
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
                        child.transition_expected(ChildLifecycle::Down {
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
            deliver_child_event(actor, ctx, &child_event, ev.incarnation).await?;
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
            decision.keep();
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
            Routed::Queued => {
                decision.keep();
                continue;
            }
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
                    .and_then(|child| child.name.as_deref().map(str::to_string));
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
                    child_id: ev.child_id,
                    child_name,
                    reason: ev.reason,
                    action,
                };
                deliver_child_event(actor, ctx, &child_event, ev.incarnation).await?;
                continue;
            }
            Routed::ChainRetry => {
                // The in-flight member's restart failed (factory panic,
                // spawn error, or the fresh incarnation died before
                // adoption): the whole group is laid out again, charging the
                // budget once (OTP try_again_restart parity).
                let charge = ctx
                    .supervision_mut()
                    .map(|sup| sup.budget.check_and_record());
                if let Some(charge) = &charge {
                    emit_restart_budget(ctx, &ev.child_id, ev.incarnation, charge);
                }
                let budget_ok = charge.is_some_and(|charge| charge.accepted);
                if budget_ok {
                    retry_group(actor, ctx, work, &ev).await?;
                    continue;
                }
                let child_name = ctx
                    .supervision_ref()
                    .and_then(|sup| sup.registry.get(&ev.child_id))
                    .and_then(|child| child.name.as_deref().map(str::to_string));
                let child_event = ChildEvent {
                    child_id: ev.child_id,
                    child_name,
                    reason: ev.reason,
                    action: SupervisionAction::BudgetExhausted,
                };
                deliver_child_event(actor, ctx, &child_event, ev.incarnation).await?;
                ctx.record_failure(SupervisionError::BudgetExhausted.into());
                return ControlFlow::Break(StopReason::ParentRequest);
            }
            Routed::Evaluate => {}
        }

        // Normal path: the ledger holds the end; evaluate the strategy, act.
        let child_name = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&ev.child_id))
            .and_then(|child| child.name.as_deref().map(str::to_string));

        // The supervisor's report of the death, before the decision it leads
        // to, as Erlang/OTP's `do_restart` writes it.
        if cfg!(feature = "tracing") && reports_death {
            if let Some((restart_type, sensitive)) = ctx
                .supervision_ref()
                .and_then(|sup| sup.registry.get(&ev.child_id))
                .map(|child| (child.spec.restart_type, child.spec.sys.sensitive))
            {
                reports::child_terminated(
                    ctx.actor_id(),
                    &ev.child_id,
                    ev.incarnation,
                    restart_type,
                    &ev.reason,
                    sensitive,
                );
            }
        }

        let mut charged = None;
        let outcome = ctx.supervision_mut().map(|sup| {
            evaluate_strategy_charged(sup, &ev.child_id, &ev.reason, &mut |incarnation, charge| {
                charged = Some((incarnation, charge));
            })
        });
        if let Some((incarnation, charge)) = &charged {
            emit_restart_budget(ctx, &ev.child_id, *incarnation, charge);
        }

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
                    sup.note_group();
                    advance_stop_queue(sup);
                }
                SupervisionAction::RestartInitiated
            }
            Some(StrategyOutcome::Remove) => {
                // Read before the spec is pruned: the end is one the
                // supervisor did not order, so it may shut the supervisor
                // down.
                let due = ctx
                    .supervision_ref()
                    .is_some_and(|sup| sup.auto_shutdown_due(&ev.child_id));
                removed = prune_spec_if_needed(ctx, &ev.child_id);
                if due {
                    SupervisionAction::AutoShutdown
                } else {
                    SupervisionAction::Removed
                }
            }
            Some(StrategyOutcome::BudgetExhausted) => SupervisionAction::BudgetExhausted,
        };
        if let Some(err) = drop_removed(ctx, removed) {
            owe_report(ctx, ev, child_name);
            return ControlFlow::Break(StopReason::Failure(err));
        }

        let child_event = ChildEvent {
            child_id: ev.child_id,
            child_name,
            reason: ev.reason,
            action,
        };
        deliver_child_event(actor, ctx, &child_event, ev.incarnation).await?;

        if matches!(action, SupervisionAction::BudgetExhausted) {
            ctx.record_failure(SupervisionError::BudgetExhausted.into());
            // OTP parity: an intensity-exceeded supervisor exits with
            // `shutdown` (our ParentRequest), so its own supervisor's
            // Transient policy does not restart it. Direct break: not
            // vetoable by pre_stop.
            return ControlFlow::Break(StopReason::ParentRequest);
        }

        if matches!(action, SupervisionAction::AutoShutdown) {
            // The supervisor shuts down as OTP's `shutdown` exit (our
            // ParentRequest), with nothing recorded as a failure: its own
            // supervisor's Transient policy does not restart it. Direct
            // break, not vetoable by pre_stop; the deaths still pending are
            // delivered as `SupervisorStopping` before `on_stopped`.
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
                    .and_then(|child| child.name.as_deref().map(str::to_string)),
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
        sup.note_group();
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
        sup.note_group();
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
    let stopped = matches!(outcome, RestartOutcome::Stopped { .. });
    match outcome {
        RestartOutcome::Adopted {
            child_id,
            incarnation,
            new_stop_lane,
            new_join,
            guard,
            io,
            ended,
        } => {
            adopt_restart(
                actor,
                ctx,
                child_id,
                incarnation,
                new_stop_lane,
                new_join,
                ended,
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
            if let Some(sup) = ctx.supervision_mut() {
                sup.settle_attempt(&child_id, incarnation, || {
                    if stopped {
                        AttemptOutcome::Abandoned
                    } else {
                        AttemptOutcome::Failed {
                            reason: reason.clone(),
                        }
                    }
                });
            }
            // An attempt that failed to start is a `start_error` of the
            // supervisor; one that ended under a stop order is not.
            if cfg!(feature = "tracing") && !stopped {
                let sensitive = ctx
                    .supervision_ref()
                    .and_then(|sup| sup.registry.get(&child_id))
                    .is_some_and(|child| child.spec.sys.sensitive);
                reports::start_error(
                    ctx.actor_id(),
                    &child_id,
                    Some(incarnation),
                    || reason.clone(),
                    sensitive,
                );
            }
            // An attempt's incarnation that ended under a stop order is
            // reported like a failed one: the end of the incarnation the
            // ledger holds as the attempt's.
            let event = ChildStoppedInternal {
                child_id,
                reason,
                incarnation,
            };
            handle_child_stopped_with(actor, ctx, event, false).await
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
                sup.settle_attempt(&child_id, incarnation, || AttemptOutcome::Abandoned);
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
            let err: ActorError = SupervisionError::ChildUnresponsive(child_id.clone()).into();
            if let Some(sup) = ctx.supervision_mut() {
                sup.settle_attempt(&child_id, incarnation, || AttemptOutcome::Failed {
                    reason: StopReason::Failure(err.clone()),
                });
                record_cancelled(sup, &child_id, incarnation);
            }
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
            child.transition_expected(ChildLifecycle::Down {
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
    ended: Option<StopReason>,
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
    if let Some(sup) = ctx.supervision_mut() {
        // The runner's claim comes before the attempt settles: an adopted
        // incarnation that already ended is owed from here, so the attempt
        // in flight and the death owed are never both absent.
        if accepted {
            claim_death(
                sup.registry.owner_record().as_ref(),
                &child_id,
                seq,
                new_stop_lane.abort_record(),
            );
        }
        sup.settle_attempt(&child_id, seq, || {
            if accepted {
                AttemptOutcome::Adopted
            } else {
                AttemptOutcome::Abandoned
            }
        });
        #[cfg(test)]
        end_hooks::run(end_hooks::SETTLED, &child_id);
    }
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
    // A restart the supervisor decided is reported at info, one the caller
    // asked for with `restart_child` at debug.
    if cfg!(feature = "tracing") {
        let by_hand = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&child_id))
            .is_some_and(|child| child.origin == Origin::Manual);
        reports::progress(
            ctx.actor_id(),
            &child_id,
            seq,
            if by_hand {
                reports::Progress::Started
            } else {
                reports::Progress::Restarted
            },
        );
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
        let owner = sup.registry.owner_record();
        let task = spawn_runner(
            &mut sup.death_set,
            child_id.clone(),
            seq,
            new_join,
            ended,
            new_stop_lane,
            io,
            guard,
            kill_grace,
            owner,
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
    let flow = handle_worklist(actor, ctx, None, &mut released).await;
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
        let name = child.name.as_deref().map(str::to_string);
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
                    // Absorbed into the group's restart: no decision of its
                    // own is owed.
                    sup.death_decided(report.incarnation);
                    if let Some(child) = sup.registry.get_mut(id) {
                        child.transition_expected(ChildLifecycle::Down {
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
                    child.transition_expected(ChildLifecycle::Down {
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
    while let Some((event, incarnation)) = deliveries.pop_front() {
        if let ControlFlow::Break(reason) =
            deliver_child_event(actor, ctx, &event, incarnation).await
        {
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
    let owner = ctx.owner();
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
    ctx.initiate_attempt(child_id, seq, origin);
}

/// A pending group restart that can make progress only through members that
/// carry the Unresponsive verdict ends the supervisor: it records and stops
/// with `ChildUnresponsive` for the first such member in reverse start order,
/// so its own parent decides. So does a stop in progress whose occupant
/// carries the verdict when no caller waits on it or was told (`orphans`,
/// read when a verdict woke the loop): a `begin_stop_child`, a
/// `begin_terminate_child`, or an awaited stop whose caller dropped its
/// future. A verdict a caller waits for, or was told, is that caller's to
/// decide.
fn check_verdicts<A: Actor>(ctx: &mut ActorContext<A>, orphans: bool) -> Option<StopReason> {
    let sup = ctx.supervision_ref()?;
    let id = group_blocked_verdict(sup).or_else(|| {
        if orphans {
            unreported_stop_verdict(sup)
        } else {
            None
        }
    })?;
    let err: ActorError = SupervisionError::ChildUnresponsive(id).into();
    ctx.record_failure(err.clone());
    Some(StopReason::Failure(err))
}

/// The member the pending group restart is blocked on, when it can make
/// progress only through members that carry the verdict.
///
/// In the stop phase the members are those the barrier still waits for; in
/// the chain phase it is the front member's own restart attempt.
fn group_blocked_verdict(sup: &SupervisionState) -> Option<ActorId> {
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
    rules::barrier_blocked_only_by_verdicts(&entries)
}

/// The child, newest first, whose stop in progress carries the Unresponsive
/// verdict with no caller to report it.
fn unreported_stop_verdict(sup: &SupervisionState) -> Option<ActorId> {
    // Newest first, each entry read only when the scan reaches it.
    rules::unreported_verdict(sup.registry.iter().rev().map(|child| {
        let stopping = matches!(child.lifecycle, ChildLifecycle::Stopping { .. });
        (
            &child.id,
            rules::StopVerdictFact {
                stopping,
                verdict: stopping && child.ctl.is_unresponsive(),
                ended: child.ctl.fate().is_some() || child.ctl.status_ended(),
                audience: child.ctl.verdict_has_audience(),
            },
        )
    }))
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

/// Logs, at error level on the `tokio_actors::supervisor` target, the end of a
/// child its supervisor's teardown stopped, when the end is not an orderly
/// one. Changes no state and runs no callback.
#[allow(unused_variables)]
fn report_unexpected_end(
    supervisor: &ActorId,
    child: &ActorId,
    incarnation: u64,
    reason: &StopReason,
    restart_type: RestartType,
    shutdown: Shutdown,
    sensitive: bool,
) {
    if !rules::teardown_end_is_orderly(reason, restart_type, shutdown) {
        #[cfg(feature = "tracing")]
        caught_log!(error,
            target: "tokio_actors::supervisor",
            report = "shutdown_error",
            supervisor = %supervisor,
            child = %child,
            incarnation,
            reason = %crate::telemetry::LogFields::of_sensitive(sensitive).reason_text(reason),
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
    supervisor: &Owner,
    outcome: RestartOutcome,
    warn: bool,
    drop_panic: &mut Option<String>,
) -> (ActorId, u64, Option<AdoptedRunner>, Option<StopReason>) {
    let stopped = matches!(outcome, RestartOutcome::Stopped { .. });
    match outcome {
        RestartOutcome::Adopted {
            child_id,
            incarnation,
            new_stop_lane,
            new_join,
            guard,
            io,
            ended,
        } => {
            let accepted = sup.registry.get(&child_id).is_some_and(|child| {
                matches!(child.lifecycle, ChildLifecycle::Restarting { next, .. } if next == incarnation)
            });
            // Claimed before the settle, as an adoption in the loop is.
            if accepted {
                claim_death(
                    sup.registry.owner_record().as_ref(),
                    &child_id,
                    incarnation,
                    new_stop_lane.abort_record(),
                );
            }
            sup.settle_attempt(&child_id, incarnation, || {
                if accepted {
                    AttemptOutcome::Adopted
                } else {
                    AttemptOutcome::Abandoned
                }
            });
            #[cfg(test)]
            end_hooks::run(end_hooks::SETTLED, &child_id);
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
            let owner = sup.registry.owner_record();
            let task = spawn_runner(
                &mut sup.death_set,
                child_id.clone(),
                incarnation,
                new_join,
                ended,
                new_stop_lane,
                io,
                guard,
                kill_grace,
                owner,
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
            sup.settle_attempt(&child_id, incarnation, || {
                if stopped {
                    AttemptOutcome::Abandoned
                } else {
                    AttemptOutcome::Failed {
                        reason: reason.clone(),
                    }
                }
            });
            if let (true, Some(child)) = (warn, sup.registry.get(&child_id)) {
                report_unexpected_end(
                    supervisor,
                    &child_id,
                    incarnation,
                    &reason,
                    child.spec.restart_type,
                    child.spec.shutdown,
                    child.spec.sys.sensitive,
                );
            }
            (child_id, incarnation, None, Some(reason))
        }
        RestartOutcome::Cancelled {
            child_id,
            incarnation,
            drop_panic: panic,
        } => {
            sup.settle_attempt(&child_id, incarnation, || AttemptOutcome::Abandoned);
            record_cancelled(sup, &child_id, incarnation);
            keep_first_of(drop_panic, panic, supervisor);
            (child_id, incarnation, None, None)
        }
        RestartOutcome::PredecessorUnresponsive {
            child_id,
            incarnation,
        } => {
            sup.settle_attempt(&child_id, incarnation, || AttemptOutcome::Failed {
                reason: StopReason::Failure(
                    SupervisionError::ChildUnresponsive(child_id.clone()).into(),
                ),
            });
            record_cancelled(sup, &child_id, incarnation);
            (child_id, incarnation, None, None)
        }
    }
}

#[cfg(test)]
#[path = "end_order_tests.rs"]
mod end_order_tests;

/// How many turns the calling thread's run loops took the turn boundary fast
/// path.
#[cfg(test)]
pub(crate) mod fast_path_turns {
    use std::cell::Cell;

    thread_local! {
        static TURNS: Cell<u32> = const { Cell::new(0) };
    }

    pub(super) fn bump() {
        TURNS.with(|t| t.set(t.get() + 1));
    }

    pub(crate) fn read() -> u32 {
        TURNS.with(Cell::get)
    }
}

/// The worker count and the current-thread flag each actor's run loop
/// started with.
#[cfg(test)]
pub(crate) mod loop_facts {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use crate::types::ActorId;

    static FACTS: OnceLock<Mutex<HashMap<ActorId, (usize, bool)>>> = OnceLock::new();

    fn facts() -> &'static Mutex<HashMap<ActorId, (usize, bool)>> {
        FACTS.get_or_init(Default::default)
    }

    pub(super) fn record(id: &ActorId, workers: usize, same_thread: bool) {
        facts()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.clone(), (workers, same_thread));
    }

    /// What the latest run loop of `id` started with, if one has.
    pub(crate) fn read(id: &ActorId) -> Option<(usize, bool)> {
        facts()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .copied()
    }

    /// Forgets what `id`'s run loops started with.
    pub(crate) fn forget(id: &ActorId) {
        facts().lock().unwrap_or_else(|p| p.into_inner()).remove(id);
    }
}

/// How many times each actor's run loop handed the thread back after a
/// reply.
#[cfg(test)]
pub(crate) mod hand_backs {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use crate::types::ActorId;

    static COUNTS: OnceLock<Mutex<HashMap<ActorId, u32>>> = OnceLock::new();

    fn counts() -> &'static Mutex<HashMap<ActorId, u32>> {
        COUNTS.get_or_init(Default::default)
    }

    pub(super) fn bump(id: &ActorId) {
        *counts()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(id.clone())
            .or_insert(0) += 1;
    }

    pub(crate) fn read(id: &ActorId) -> u32 {
        counts()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .copied()
            .unwrap_or(0)
    }
}

#[cfg(test)]
#[path = "hand_back_flavor_tests.rs"]
mod hand_back_flavor_tests;

#[cfg(test)]
#[path = "runtime_facts_tests.rs"]
mod runtime_facts_tests;

#[cfg(test)]
#[path = "ack_waker_tests.rs"]
mod ack_waker_tests;

#[cfg(test)]
#[path = "status_sender_tests.rs"]
mod status_sender_tests;

#[cfg(test)]
#[path = "status_fast_path_tests.rs"]
mod status_fast_path_tests;

#[cfg(test)]
#[path = "task_key_tests.rs"]
mod task_key_tests;

#[cfg(test)]
#[path = "bounded_run.rs"]
mod bounded_run;

#[cfg(test)]
#[path = "system_hold_tests.rs"]
mod system_hold_tests;

#[cfg(test)]
#[path = "own_unwind_tests.rs"]
pub(crate) mod own_unwind_tests;

#[cfg(test)]
#[path = "worklist_order_tests.rs"]
mod worklist_order_tests;

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
        let owner = Owner::new(ActorId::from(format!("keep-first-{token}")), false);
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
                significant: false,
                sys: crate::actor::sys::SysOptions::default(),
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
            ended: None,
        };
        let supervisor = Owner::new(ActorId::from("supervisor"), false);
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
            child.transition_expected(ChildLifecycle::Down {
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
    let supervisor = ctx.owner();
    match joined {
        Ok((task, outcome)) => {
            let sup = ctx.supervision_mut()?;
            // An attempt the final collection reported from its status plane
            // was not stopped by the teardown: its end is not logged as one.
            let warn = !sup
                .task_owner
                .remove(&task)
                .is_some_and(|(child, incarnation, _)| sup.reported_early(&child, incarnation));
            let (child, incarnation, adopted, ended) =
                record_restart_outcome(sup, &supervisor, outcome, warn, drop_panic);
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
/// settles it, whether the incarnation is a restart attempt, whether the
/// final collection already collected its end (so the end is not logged as
/// one of a stop the teardown made), and the watch on its slot's cell.
struct StopWait {
    id: ActorId,
    token: u64,
    attempt: bool,
    collected: bool,
    watch: CellWatch,
}

/// A wait on a slot's cell that owns its watch.
type CellWait = Pin<Box<dyn Future<Output = CellOutcome> + Send>>;

/// The waits of a teardown, each polled only after its own waker fired, so
/// one settled wait costs one poll of that wait and not a poll of every wait
/// still pending.
///
/// Each wait gets a waker of its own ([`WaitWaker`]). Its wake puts the
/// wait's index on the shared queue, once until the wait is polled again, and
/// wakes the task that polls the set, inside the crate's catch. A poll takes
/// the queue and polls only those waits. The queue's lock is never held
/// across a poll of a wait or a wake.
struct SettleSet {
    waits: Vec<Option<CellWait>>,
    wakers: Vec<(Arc<WaitWaker>, std::task::Waker)>,
    woken: Arc<Woken>,
    pending: usize,
    #[cfg(test)]
    polls: usize,
}

/// The indices of the waits woken since the set last took them, and the
/// waker of the task that polls the set.
#[derive(Default)]
struct Woken(std::sync::Mutex<WokenQueue>);

#[derive(Default)]
struct WokenQueue {
    indices: VecDeque<usize>,
    parent: Option<std::task::Waker>,
}

impl Woken {
    fn queue(&self) -> std::sync::MutexGuard<'_, WokenQueue> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The waker of one wait of a [`SettleSet`].
struct WaitWaker {
    index: usize,
    /// Whether the index is on the queue and the wait not yet polled since.
    queued: AtomicBool,
    woken: Arc<Woken>,
}

impl std::task::Wake for WaitWaker {
    fn wake(self: Arc<Self>) {
        self.put_on_queue();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.put_on_queue();
    }
}

impl WaitWaker {
    /// Queues the wait's index, unless it is queued already, and wakes the
    /// task that polls the set.
    fn put_on_queue(&self) {
        if self.queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let parent = {
            let mut queue = self.woken.queue();
            queue.indices.push_back(self.index);
            queue.parent.clone()
        };
        if let Some(parent) = parent {
            contain_waker_panic("teardown wait", None, move || parent.wake());
        }
    }
}

impl SettleSet {
    /// Every wait starts on the queue, so its first poll registers it.
    fn new(waits: Vec<CellWait>) -> Self {
        let woken = Arc::new(Woken::default());
        let wakers = (0..waits.len())
            .map(|index| {
                let waker = Arc::new(WaitWaker {
                    index,
                    queued: AtomicBool::new(true),
                    woken: woken.clone(),
                });
                let handle = std::task::Waker::from(waker.clone());
                (waker, handle)
            })
            .collect();
        woken.queue().indices.extend(0..waits.len());
        Self {
            pending: waits.len(),
            waits: waits.into_iter().map(Some).collect(),
            wakers,
            woken,
            #[cfg(test)]
            polls: 0,
        }
    }

    /// The waits that have not settled.
    fn pending(&self) -> usize {
        self.pending
    }

    /// Resolves with the index and outcome of a wait that settled, and drops
    /// it. Pending while none of the woken waits has.
    fn next_settled(&mut self) -> impl Future<Output = (usize, CellOutcome)> + '_ {
        std::future::poll_fn(move |cx| {
            let mut taken = {
                let mut queue = self.woken.queue();
                if !queue
                    .parent
                    .as_ref()
                    .is_some_and(|parent| parent.will_wake(cx.waker()))
                {
                    queue.parent = Some(cx.waker().clone());
                }
                std::mem::take(&mut queue.indices)
            };
            while let Some(index) = taken.pop_front() {
                let (flag, waker) = &self.wakers[index];
                // Cleared before the poll: a wake from here on queues the
                // wait again.
                flag.queued.swap(false, Ordering::AcqRel);
                let Some(wait) = self.waits[index].as_mut() else {
                    continue;
                };
                #[cfg(test)]
                {
                    self.polls += 1;
                }
                let mut wait_cx = std::task::Context::from_waker(waker);
                if let Poll::Ready(outcome) = wait.as_mut().poll(&mut wait_cx) {
                    self.waits[index] = None;
                    self.pending -= 1;
                    if !taken.is_empty() {
                        // Not polled yet: still queued, so put them back first.
                        let mut queue = self.woken.queue();
                        taken.append(&mut queue.indices);
                        queue.indices = taken;
                    }
                    return Poll::Ready((index, outcome));
                }
            }
            Poll::Pending
        })
    }
}

/// Waits for the given stops to settle and records the ends. The restart
/// plane is drained meanwhile: an attempt ordered to stop reports on it, and
/// an outcome that waited there for the supervisor to adopt it is adopted, so
/// its runner honors the order. Each wait ends with its cell's settlement
/// and, for an attempt, the drain of its own report, or the absence of any
/// restart-plane task working for it (its report was taken before the wait);
/// an Unresponsive verdict ends it at once. A factory drop panic a drained
/// attempt carries is kept in `drop_panic`.
///
/// No restart-plane task is started while the teardown waits (the factories
/// are gone), so the restart-plane keys of the targets, read once here, change
/// only when this wait drains one of their tasks, and a counter of the
/// targets not yet done replaces a scan of every target at every wake.
async fn settle_children<A: Actor>(
    ctx: &mut ActorContext<A>,
    waits: Vec<StopWait>,
    stop_rx: &mut LaneRx,
    drop_panic: &mut Option<String>,
) -> Teardown {
    let supervisor = ctx.actor_id().clone();
    let mut targets: Vec<(ActorId, u64, bool)> = Vec::with_capacity(waits.len());
    let mut cells: Vec<CellWait> = Vec::with_capacity(waits.len());
    let mut reported = Vec::with_capacity(waits.len());
    for wait in waits {
        let StopWait {
            id,
            token,
            attempt,
            collected,
            mut watch,
        } = wait;
        targets.push((id, token, attempt));
        reported.push(collected);
        cells.push(Box::pin(async move { await_cell(&mut watch, token).await }));
    }
    let mut cells = SettleSet::new(cells);
    let mut settled: Vec<Option<CellOutcome>> = targets.iter().map(|_| None).collect();
    let mut done = vec![false; targets.len()];
    let mut remaining = targets.len();
    // Every target by its key, and the keys a restart-plane task works for.
    let mut by_key: HashMap<(ActorId, u64), Vec<usize>> = HashMap::new();
    for (index, (id, token, _)) in targets.iter().enumerate() {
        by_key.entry((id.clone(), *token)).or_default().push(index);
    }
    let mut restart_keys: HashSet<(ActorId, u64)> = ctx
        .supervision_ref()
        .map(|sup| {
            sup.task_owner
                .values()
                .filter(|(owner, incarnation, plane)| {
                    *plane == TaskPlane::Restart
                        && by_key.contains_key(&(owner.clone(), *incarnation))
                })
                .map(|(owner, incarnation, _)| (owner.clone(), *incarnation))
                .collect()
        })
        .unwrap_or_default();
    // A target is done once its cell settled and, for an attempt, its own
    // report was drained, or no task of the restart plane works for it any
    // more (its report was taken before this wait: the final collection
    // takes it); an Unresponsive verdict ends it at once.
    let finished = |index: usize,
                    targets: &[(ActorId, u64, bool)],
                    settled: &[Option<CellOutcome>],
                    restart_keys: &HashSet<(ActorId, u64)>| {
        let (id, token, attempt) = &targets[index];
        settled[index].as_ref().is_some_and(|outcome| {
            !attempt
                || matches!(outcome, CellOutcome::Unresponsive)
                || !restart_keys.contains(&(id.clone(), *token))
        })
    };
    if kill_stored(stop_rx) {
        return Teardown::Killed;
    }
    while let Some(sup) = ctx.supervision_mut() {
        if remaining == 0 {
            break;
        }
        let restart_pending = !sup.restart_set.is_empty();
        let cells_pending = cells.pending() > 0;
        tokio::select! {
            biased;
            // A `Kill` raised while the teardown waits turns the rest of it
            // into the `Kill` path. Only a stored `Kill` ends the wait: the
            // lane holds nothing else new once it is narrowed, and a version
            // the loop never consumed is consumed here.
            changed = stop_rx.changed() => {
                match changed {
                    Ok(()) if kill_stored(stop_rx) => return Teardown::Killed,
                    Ok(()) => {}
                }
            }
            (index, outcome) = cells.next_settled(), if cells_pending => {
                settled[index] = Some(outcome);
                if !done[index] && finished(index, &targets, &settled, &restart_keys) {
                    done[index] = true;
                    remaining -= 1;
                }
            }
            joined = sup.restart_set.join_next_with_id(), if restart_pending => {
                if let Some(joined) = joined {
                    let task = match &joined {
                        Ok((task, _)) => *task,
                        Err(err) => err.id(),
                    };
                    let joined_key = sup
                        .task_owner
                        .get(&task)
                        .filter(|(_, _, plane)| *plane == TaskPlane::Restart)
                        .map(|(owner, incarnation, _)| (owner.clone(), *incarnation));
                    let drained = drain_restart_for_teardown(ctx, joined, drop_panic);
                    let mut touched: Vec<usize> = Vec::new();
                    if let Some(key) = joined_key {
                        restart_keys.remove(&key);
                        touched.extend(by_key.get(&key).into_iter().flatten().copied());
                    }
                    if let Some((child, incarnation, adopted)) = drained {
                        let key = (child, incarnation);
                        restart_keys.remove(&key);
                        for &index in by_key.get(&key).into_iter().flatten() {
                            targets[index].2 = false;
                            reported[index] |= !adopted;
                            touched.push(index);
                        }
                    }
                    for index in touched {
                        if !done[index] && finished(index, &targets, &settled, &restart_keys) {
                            done[index] = true;
                            remaining -= 1;
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
            report_unexpected_end(
                &supervisor,
                id,
                *token,
                &fate.reason,
                child.spec.restart_type,
                child.spec.shutdown,
                child.spec.sys.sensitive,
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
        collected: false,
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
    stop_rx: &mut LaneRx,
    drop_panic: &mut Option<String>,
) -> Teardown {
    let (strategy, order, factories) = match ctx.supervision_mut() {
        Some(sup) => {
            let factories = std::mem::take(&mut sup.restart_fns);
            sup.pending_group = None;
            sup.note_group();
            for report in std::mem::take(&mut sup.queued_triggers) {
                sup.death_decided(report.incarnation);
            }
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
    let supervisor = ctx.owner();
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
    // The incarnations whose ends the final collection took from their
    // status planes and whose tasks have not returned: the ledger holds them
    // down, and the supervisor publishes its end only once each incarnation
    // ended. No signal is raised: each is ending already. An attempt's wait
    // is bounded by the attempt's own ladder; a runner's ladder is armed with
    // the child's own policy, as the walk's order would arm it.
    let reported: Vec<StopWait> = ctx
        .supervision_ref()
        .map(|sup| {
            sup.reported_ends()
                .into_iter()
                .map(|(id, token, attempt, watch)| {
                    if !attempt {
                        if let Some(child) = sup.registry.get(&id) {
                            if let Some(ctl) = child.slot_of(token) {
                                ctl.order_bound(child.spec.shutdown);
                            }
                        }
                    }
                    StopWait {
                        id,
                        token,
                        attempt,
                        collected: true,
                        watch,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    if !reported.is_empty() {
        #[cfg(test)]
        end_hooks::run(end_hooks::REPORTED_WAIT, &supervisor);
        if settle_children(ctx, reported, stop_rx, drop_panic).await == Teardown::Killed {
            return Teardown::Killed;
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
fn kill_stored(stop_rx: &LaneRx) -> bool {
    matches!(stop_rx.borrow().reason, Some(StopReason::Kill))
}

/// How the strict start's wait for the supervisor's children ended.
enum StrictOutcome {
    /// Every child spawned while starting finished its initialization.
    Done,
    /// A `Kill` is stored on the supervisor's lane.
    Killed,
    /// A child's initialization failed: the supervisor's start fails with it.
    Failed(ActorError),
}

/// Resolves when a `Kill` is stored on the lane, without marking any version
/// seen: a request of another kind stays for the message loop, which reads it
/// after the start. The wake slot is the run loop's own, armed only while
/// this future is polled and disarmed when it drops.
struct KillStored<'a> {
    rx: &'a LaneRx,
    slot: &'a crate::types::WakeSlot,
}

impl Future for KillStored<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if kill_stored(self.rx) {
            return Poll::Ready(());
        }
        self.slot.register(cx.waker());
        if kill_stored(self.rx) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for KillStored<'_> {
    fn drop(&mut self) {
        self.slot.disarm();
    }
}

/// The strict start's wait: for each child spawned while the supervisor
/// started, in the order they were spawned, until its initialization is
/// acknowledged. The children initialize concurrently; only the wait is in
/// order. The wait has no bound of its own (the start bound of the
/// supervisor's spawn, if any, cuts it); a `Kill` ends it at once. A child
/// that fails, or ended before its initialization finished without its
/// supervisor removing, replacing or ordering that end, fails the start with
/// [`ActorError::ChildStart`]. Each acknowledgment belongs to the incarnation
/// that owes it: what the ledger holds for the child decides only for that
/// incarnation, and its failure reason is read from that incarnation's slot.
async fn await_strict_children<A: Actor>(
    ctx: &mut ActorContext<A>,
    stop_rx: &LaneRx,
) -> StrictOutcome {
    let acks = match ctx.supervision_mut() {
        Some(sup) => sup.take_strict_acks(),
        None => return StrictOutcome::Done,
    };
    let record = ctx.stop_lane().abort_record().clone();
    for (child, incarnation, mut ack) in acks {
        let answer = tokio::select! {
            biased;
            () = KillStored { rx: stop_rx, slot: record.reader() } => {
                return StrictOutcome::Killed;
            }
            answer = &mut ack => answer,
        };
        let kind = match &answer {
            Ok(Ok(())) => StrictAckKind::Ok,
            Ok(Err(_)) => StrictAckKind::Err,
            Err(_closed) => StrictAckKind::Closed {
                ordered: {
                    let held = ctx
                        .supervision_ref()
                        .and_then(|sup| sup.registry.get(&child));
                    strict_closed_ordered(
                        held.map(|state| state.lifecycle.occupant()),
                        incarnation,
                        held.and_then(|state| state.slot_of(incarnation))
                            .is_some_and(|ctl| ctl.order_before_end()),
                    )
                },
            },
        };
        if strict_step(kind) == StrictStep::Next {
            continue;
        }
        let error = match answer {
            Ok(Err(err)) => err,
            _ => {
                // The child ended before it acknowledged: its final reason is
                // the error, `Failure`'s own, else a description.
                let status = ctx
                    .supervision_ref()
                    .and_then(|sup| sup.registry.get(&child))
                    .and_then(|state| state.slot_of(incarnation))
                    .and_then(|ctl| ctl.status());
                let reason = match status {
                    Some(mut status) => {
                        tokio::select! {
                            biased;
                            () = KillStored { rx: stop_rx, slot: record.reader() } => {
                                return StrictOutcome::Killed;
                            }
                            reason = crate::system::wait_final_reason(&mut status) => reason,
                        }
                    }
                    None => StopReason::Kill,
                };
                match reason {
                    StopReason::Failure(err) => err,
                    other => ActorError::user(format!("child ended during start: {other}")),
                }
            }
        };
        return StrictOutcome::Failed(ActorError::ChildStart {
            child,
            error: Box::new(error),
        });
    }
    StrictOutcome::Done
}

/// Pushes one collected report, with the child's name as the ledger (or a
/// pending snapshot of the end) holds it, and records the end in the ledger.
fn collect_report(
    sup: &mut SupervisionState,
    out: &mut Vec<Collected>,
    report: ChildStoppedInternal,
) {
    // Collected for the final deliveries: no restart is owed for it.
    sup.death_decided(report.incarnation);
    if !sup.silenced.is_empty() && sup.take_silenced(&report.child_id, report.incarnation) {
        return;
    }
    let name = sup
        .registry
        .get(&report.child_id)
        .and_then(|child| child.name.as_deref().map(str::to_string))
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

/// As [`collect_report`], for the report a runner returned to the final
/// collection: dropped when the collection already took that incarnation's
/// death from its status plane (`early`), so each death is collected once.
fn collect_runner(
    sup: &mut SupervisionState,
    out: &mut Vec<Collected>,
    early: &HashSet<(ActorId, u64)>,
    report: ChildStoppedInternal,
) {
    if early.contains(&(report.child_id.clone(), report.incarnation)) {
        return;
    }
    collect_report(sup, out, report);
}

/// Collects, delivering nothing yet, every report of an incarnation's end
/// that was still pending when the supervisor's message loop ended.
///
/// The runners of the incarnations whose tasks had already ended, and the
/// attempts whose incarnations had, are awaited first: a runner writes its
/// fate and returns after the task published `Stopped`, so reading the
/// planes at once would miss a child that ended just before the loop did.
/// The set collected is fixed by this collection: an incarnation whose end
/// it does not see is the teardown's. An incarnation that ended on its own,
/// its final reason written, while its task has not published its end (it
/// may still be dropping what it holds) is collected as it would be once
/// the task ended, without waiting for the task, which the teardown then
/// waits for: an attempt whose start failed, was cut or was ordered to stop
/// is reported from that reason with the outcome the attempt reports
/// itself; an attempt whose start completed hands the incarnation over, the
/// collection awaiting the attempt, which returns at its next poll; and a
/// runner's incarnation has its death collected with that reason. Then, in
/// this order: the worklist a
/// completing group left, the deferred triggers, the restart plane (an
/// adopted attempt is adopted, nothing is delivered for it), the attempts
/// reported from their status planes, and the death
/// plane, each read until empty, the death plane then read from the status
/// planes of the runners it still holds. Each collected report is recorded as the
/// teardown records a drain: the ledger reaches `Down` with nothing owed, a
/// snapshot of the end is dropped, a barrier entry is released. No strategy,
/// charge, restart or group step runs.
async fn collect_final<A: Actor>(
    ctx: &mut ActorContext<A>,
    drop_panic: &mut Option<String>,
) -> Vec<Collected> {
    let supervisor = ctx.owner();
    let mut out: Vec<Collected> = Vec::new();
    let Some(sup) = ctx.supervision_ref() else {
        return out;
    };
    let crate::actor::supervision::FinalSets {
        mut runners,
        mut attempts,
        adopts,
        mut reported,
    } = sup.final_await_sets();
    // Runners of adopted attempts whose incarnation ended in transit.
    let mut adopted: std::collections::HashSet<tokio::task::Id> = Default::default();
    // The attempts that hand their incarnation over at their next poll: the
    // wait on one ends early when its cell settles first.
    let mut adopting: Vec<(tokio::task::Id, ActorId, u64)> = Vec::with_capacity(adopts.len());
    let mut cells: Vec<CellWait> = Vec::with_capacity(adopts.len());
    for (task, child_id, incarnation, mut watch) in adopts {
        adopting.push((task, child_id, incarnation));
        cells.push(Box::pin(async move {
            await_cell(&mut watch, incarnation).await
        }));
    }
    let mut cells = SettleSet::new(cells);

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

    // The attempts whose incarnations ended, and those that hand their
    // incarnation over.
    #[cfg(test)]
    if !attempts.is_empty() {
        end_hooks::run(end_hooks::ATTEMPT_WAIT, &supervisor);
    }
    while !attempts.is_empty() {
        let Some(sup) = ctx.supervision_mut() else {
            break;
        };
        let watching = cells.pending() > 0;
        tokio::select! {
            biased;
            joined = sup.restart_set.join_next_with_id() => match joined {
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
            },
            // The cell of an attempt read as handing its incarnation over
            // settled before the attempt returned: its ladder left its idle
            // step meanwhile, so it hands nothing over, and it is reported
            // from its incarnation's final reason, as it reports itself.
            (index, _) = cells.next_settled(), if watching => {
                let (task, child_id, incarnation) = &adopting[index];
                if attempts.remove(task) {
                    let outcome = sup
                        .registry
                        .get(child_id)
                        .and_then(|child| child.slot_of(*incarnation))
                        .and_then(|ctl| crate::actor::supervision::slot::settled_attempt(
                            ctl,
                            child_id,
                            *incarnation,
                        ));
                    if let Some(outcome) = outcome {
                        reported.push((*task, outcome));
                    }
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

    // The attempts whose incarnation's start failed with its final reason
    // written and whose task had not published its end: each is reported
    // with the outcome the attempt reports itself, unless a join above
    // already took it. Its task stays registered, so the teardown waits for
    // it; nothing here waits for the incarnation's drops.
    if let Some(sup) = ctx.supervision_mut() {
        for (task, outcome) in reported {
            if sup.task_owner.contains_key(&task) {
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

    // The death plane: the runners the adoptions above spawned for
    // incarnations that ended in transit first, then everything it holds.
    // Once it holds no runner that has returned, the runners left are read
    // from their status planes: a runner whose incarnation published its
    // end is awaited, and an incarnation that ended on its own with its task
    // still dropping has its death collected with its final reason, without
    // waiting for the task. Each pass that collects or awaits anything is
    // followed by another. A death collected from its plane is recorded in
    // `early`, keyed by child and incarnation, and its runner's own report,
    // when the collection joins it, is dropped.
    let mut early: HashSet<(ActorId, u64)> = HashSet::new();
    loop {
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
                        collect_runner(
                            sup,
                            &mut out,
                            &early,
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
                    collect_runner(
                        sup,
                        &mut out,
                        &early,
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
        let Some(sup) = ctx.supervision_mut() else {
            break;
        };
        let (ended, deaths) = sup.final_deaths(&early);
        if ended.is_empty() && deaths.is_empty() {
            break;
        }
        adopted.extend(ended);
        for report in deaths {
            let key = (report.child_id.clone(), report.incarnation);
            collect_report(sup, &mut out, report);
            #[cfg(test)]
            end_hooks::run(end_hooks::PLANE_REPORT, &key.0);
            early.insert(key);
        }
    }

    // Nothing of the group is left to do, and `children()` inside a final
    // delivery reads no member as awaiting a restart that will not be made.
    if let Some(sup) = ctx.supervision_mut() {
        sup.pending_group = None;
        sup.note_group();
        // An attempt that reached no outcome ends with the supervisor: it
        // reports before the supervisor's own end event.
        sup.verdicts.settle_unsettled();
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
    supervisor: &Owner,
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
    stop_rx: &LaneRx,
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
        emit_child_died(ctx, &event, report.incarnation);
        if let Some(sup) = ctx.supervision_mut() {
            sup.final_delivery = true;
        }
        let turn = clock.begin();
        let result = catch_hook_t(clock.caller_token(turn), || {
            actor.on_child_stopped(&event, ctx)
        })
        .await;
        let took = clock.end_timed(turn);
        push_turn(
            ctx.stop_lane().abort_record(),
            turn,
            took,
            HookKind::OnChildStopped,
            outcome_of(&result),
        );
        if let Some(sup) = ctx.supervision_mut() {
            sup.final_delivery = false;
        }
        // The chain does not run after this callback: a message it asked for
        // is dropped here, not carried to a later delivery or to the end.
        ctx.drop_continuation();
        // An earlier failure stands, whether the delivery returns an error or
        // panics.
        let failed = matches!(accepted, StopReason::Failure(_))
            || matches!(pending, Some(StopReason::Failure(_)));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                ctx.record_failure(err.clone());
                if !failed {
                    pending = Some(StopReason::Failure(err));
                }
            }
            Err(payload) => {
                let err = ActorError::Panic(payload_into_string(payload));
                ctx.record_failure(err.clone());
                if !failed {
                    pending = Some(StopReason::Failure(err));
                }
            }
        }
    }
    FinalOutcome::Done { pending }
}

/// The stop-time stage for monitors, for any actor whose message loop ended
/// for a reason but `Kill`, between the loop (and a supervisor's final child
/// reports) and `on_stopped`: every notice the actor is still owed is
/// delivered once through `handle`, the status reading `Stopping`, the stop
/// lane read before each delivery, none once a `Kill` is stored. A delivery
/// that returns an `Err` or panics makes the exit reason a `Failure` once
/// `on_stopped` returns unless one already stands, and the remaining
/// deliveries still run.
///
/// Owed are the notices queued in the mailbox or the priority lane, in queue
/// order, then the monitors whose watched actor ended and whose task has not
/// queued a notice, in monitor order. The stage first closes every monitor
/// nobody has taken, so no task queues another notice from then on; a task
/// that took its notice before is waited for, a few instructions, because it
/// queues no user code. The messages the stage takes from the queues while
/// it looks are kept in the monitor state and dropped with the context.
#[allow(clippy::too_many_arguments)]
async fn monitor_drain<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    stop_rx: &LaneRx,
    accepted: &StopReason,
    turn_clock: &TurnClock,
    mailbox: &mut MailRx<ActorEnvelope<A>>,
    mut priority: Option<&mut mpsc::Receiver<ActorEnvelope<A>>>,
    drain: &mut ReplyDrain,
    held: &mut HeldAnswer<A::Response>,
) -> FinalOutcome {
    let mut seen: std::collections::HashSet<MonitorId> = std::collections::HashSet::new();
    let mut queued: Vec<ActorEnvelope<A>> = Vec::new();
    let mut owed: Vec<(MonitorId, DownReason)> = Vec::new();
    let mut link_owed: Vec<(MonitorId, LinkExit)> = Vec::new();
    loop {
        owed.extend(
            ctx.monitors_mut()
                .map(|state| state.close_pass())
                .unwrap_or_default(),
        );
        link_owed.extend(ctx.close_links());
        if !notice_in_flight(ctx, &seen) {
            break;
        }
        // Take what is queued now: at most the queued count, so a producer
        // that refills the queue cannot hold the stage.
        let mut take = mailbox.len();
        while take != 0 {
            take -= 1;
            match mailbox.try_recv_one() {
                Ok(envelope) => keep_or_queue(ctx, envelope, &mut seen, &mut queued),
                Err(_) => break,
            }
        }
        if let Some(lane) = priority.as_deref_mut() {
            let mut take = lane.len();
            while take != 0 {
                take -= 1;
                match lane.try_recv_one() {
                    Ok(envelope) => keep_or_queue(ctx, envelope, &mut seen, &mut queued),
                    Err(_) => break,
                }
            }
        }
        if !notice_in_flight(ctx, &seen) {
            break;
        }
        // A task took its notice and is putting it in the queue.
        tokio::task::yield_now().await;
    }
    let mut pending: Option<StopReason> = None;
    let failed = |pending: &Option<StopReason>| {
        matches!(accepted, StopReason::Failure(_))
            || matches!(pending, Some(StopReason::Failure(_)))
    };
    let mut queued = queued.into_iter();
    while let Some(envelope) = queued.next() {
        if kill_stored(stop_rx) {
            // The notice in hand and those behind it hold the user's
            // messages: each is dropped inside a catch, as every other value
            // the stage leaves behind is.
            drop_queued(std::iter::once(envelope).chain(queued), ctx);
            return FinalOutcome::Killed;
        }
        if let ControlFlow::Break(reason) =
            dispatch(actor, ctx, envelope, turn_clock, drain, held, None).await
        {
            if !failed(&pending) {
                pending = Some(reason);
            }
        }
    }
    for (id, reason) in owed {
        if kill_stored(stop_rx) {
            return FinalOutcome::Killed;
        }
        let made = ctx
            .monitors_mut()
            .and_then(|state| state.owed_message(id, reason));
        let outcome = match made {
            None => continue,
            Some(Ok(payload)) => {
                ctx.finish_monitor(id);
                let envelope = Envelope::Message {
                    payload,
                    responder: None,
                };
                dispatch(actor, ctx, envelope, turn_clock, drain, held, None).await
            }
            Some(Err(panic)) => {
                ctx.finish_monitor(id);
                let err = ActorError::Panic(panic);
                ctx.record_failure(err.clone());
                ControlFlow::Break(StopReason::Failure(err))
            }
        };
        if let ControlFlow::Break(reason) = outcome {
            if !failed(&pending) {
                pending = Some(reason);
            }
        }
    }
    for (id, exit) in link_owed {
        if kill_stored(stop_rx) {
            return FinalOutcome::Killed;
        }
        let made = ctx
            .monitors_mut()
            .and_then(|state| state.link_owed_message(id, exit));
        if let Some(made) = made {
            ctx.finish_monitor(id);
            if let ControlFlow::Break(reason) =
                deliver_made(actor, ctx, made, turn_clock, drain, held).await
            {
                if !failed(&pending) {
                    pending = Some(reason);
                }
            }
        }
    }
    // The exit signals queued in the inbox are the last of the stage, and
    // the inbox is sealed: a signal sent from here on is refused.
    let deliver = ctx.can_deliver_exits();
    for item in ctx.stop_lane().abort_record().take_and_seal_signals() {
        let InboxItem::Exit { from, reason, .. } = item else {
            continue;
        };
        if !deliver {
            continue;
        }
        if kill_stored(stop_rx) {
            return FinalOutcome::Killed;
        }
        let exit = LinkExit {
            actor_id: from.actor_id,
            incarnation: Some(from.incarnation),
            reason: DownReason::Stopped(reason),
        };
        let made = ctx
            .monitors_mut()
            .and_then(|state| state.exit_message(exit));
        if let Some(made) = made {
            if let ControlFlow::Break(reason) =
                deliver_made(actor, ctx, made, turn_clock, drain, held).await
            {
                if !failed(&pending) {
                    pending = Some(reason);
                }
            }
        }
    }
    if let Some(state) = ctx.monitors_mut() {
        state.clear();
    }
    FinalOutcome::Done { pending }
}

/// Drops the notices a stage leaves undelivered when a `Kill` ends it, each
/// inside a catch: the message in a notice is the user's, and its drop can
/// panic. The panic is logged with the actor's id under `tracing`, and the
/// actor still ends as `Kill`.
fn drop_queued<A: Actor>(notices: impl Iterator<Item = ActorEnvelope<A>>, ctx: &ActorContext<A>) {
    for notice in notices {
        if let Some(panic) = end_value(notice) {
            log_late_drop(&panic, &ctx.owner());
        }
    }
}

/// Delivers a message the stage made from an exit: through `handle`, or, when
/// the map panicked, as the failure of the stage.
async fn deliver_made<A: Actor>(
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    made: Result<A::Message, String>,
    turn_clock: &TurnClock,
    drain: &mut ReplyDrain,
    held: &mut HeldAnswer<A::Response>,
) -> ControlFlow<StopReason> {
    match made {
        Ok(payload) => {
            let envelope = Envelope::Message {
                payload,
                responder: None,
            };
            dispatch(actor, ctx, envelope, turn_clock, drain, held, None).await
        }
        Err(panic) => {
            let err = ActorError::Panic(panic);
            ctx.record_failure(err.clone());
            ControlFlow::Break(StopReason::Failure(err))
        }
    }
}

/// Whether a monitor's task took its notice and the stage has not yet found
/// it in a queue.
fn notice_in_flight<A: Actor>(
    ctx: &mut ActorContext<A>,
    seen: &std::collections::HashSet<MonitorId>,
) -> bool {
    ctx.monitors_mut()
        .is_some_and(|state| state.in_flight(seen))
}

/// Sorts a message the stage took from a queue: a notice of a monitor the
/// actor still holds goes to the deliveries, anything else is kept until the
/// context drops.
fn keep_or_queue<A: Actor>(
    ctx: &mut ActorContext<A>,
    envelope: ActorEnvelope<A>,
    seen: &mut std::collections::HashSet<MonitorId>,
    queued: &mut Vec<ActorEnvelope<A>>,
) {
    let Some(state) = ctx.monitors_mut() else {
        return;
    };
    match &envelope {
        Envelope::Signal { tag, .. } => {
            seen.insert(tag.shared.id());
            if state.wants(tag) {
                queued.push(envelope);
                return;
            }
        }
        Envelope::Message { .. } => {}
    }
    state.stash(envelope);
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

pub(crate) fn build_status_info<A: Actor>(
    ctx: &ActorContext<A>,
    suspended: bool,
    state: Option<String>,
) -> ActorStatusInfo {
    let (child_count, name) = ctx
        .supervision_ref()
        .map(|sup| (sup.registry.len(), ctx.actor_name().map(str::to_string)))
        .unwrap_or((0, ctx.actor_name().map(str::to_string)));
    let handle = ctx.self_handle_ref();

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
        incarnation: ctx.stop_lane().abort_record().incarnation(),
        monitors: ctx.monitor_count(),
        monitored_by: ctx.stop_lane().abort_record().monitored_by(),
        trap_exit: ctx.stop_lane().abort_record().trap(),
        links: ctx.link_count(),
        pg_groups: ctx.stop_lane().abort_record().pg_joined(),
        group_monitors: ctx.group_monitor_count(),
        aliases: ctx.alias_count(),
        requests_in_flight: ctx.request_count(),
        suspended,
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A teardown's waits are polled only after their own wakers fired: 64
    /// waits that settle one at a time, the last first, cost one poll each
    /// to register and one to settle, where polling every pending wait at
    /// every wake costs 2,080.
    #[test]
    fn a_teardown_wait_polls_only_the_waits_that_were_woken() {
        const WAITS: usize = 64;
        let mut senders = Vec::with_capacity(WAITS);
        let mut waits: Vec<CellWait> = Vec::with_capacity(WAITS);
        for _ in 0..WAITS {
            let (tx, mut rx) = oneshot::channel::<()>();
            senders.push(Some(tx));
            waits.push(Box::pin(std::future::poll_fn(move |cx| {
                Pin::new(&mut rx).poll(cx).map(|_| CellOutcome::Cancelled)
            })));
        }
        let mut set = SettleSet::new(waits);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        {
            let mut first = std::pin::pin!(set.next_settled());
            assert!(first.as_mut().poll(&mut cx).is_pending());
        }
        for index in (0..WAITS).rev() {
            let _ = senders[index].take().unwrap().send(());
            let mut next = std::pin::pin!(set.next_settled());
            match next.as_mut().poll(&mut cx) {
                Poll::Ready((settled, CellOutcome::Cancelled)) => assert_eq!(settled, index),
                other => panic!("wait {index} did not settle: {:?}", other.is_ready()),
            }
        }
        assert_eq!(set.pending(), 0);
        assert!(set.polls <= 2 * WAITS, "{} polls", set.polls);
    }

    /// An answer nobody receives is dropped inside a catch: a panic of its
    /// drop comes back as a payload, at a veto and at the seal alike.
    #[test]
    fn an_undelivered_answer_whose_drop_panics_is_reported() {
        struct Bomb;
        impl Drop for Bomb {
            fn drop(&mut self) {
                if !std::thread::panicking() {
                    panic!("answer drop");
                }
            }
        }
        let mut held = HeldAnswer::<Bomb>::new();
        let (tx, rx) = oneshot::channel();
        drop(rx);
        held.hold(tx, Ok(Bomb), &held_owner(), &AbortRecord::new());
        assert_eq!(held.release_now().as_deref(), Some("answer drop"));

        let (tx, rx) = oneshot::channel();
        drop(rx);
        held.hold(tx, Ok(Bomb), &held_owner(), &AbortRecord::new());
        assert_eq!(
            held.deliver(&StopReason::Graceful).as_deref(),
            Some("answer drop")
        );

        // A task that ends before the seal drops the held answer inside the
        // catch: the panic of its drop does not leave the drop.
        let (tx, rx) = oneshot::channel();
        drop(rx);
        held.hold(tx, Ok(Bomb), &held_owner(), &AbortRecord::new());
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(held)));
        assert!(ended.is_ok(), "the held value's drop panic was contained");
    }

    /// The id a held answer of these tests is logged under.
    fn held_owner() -> ActorId {
        ActorId::from("held")
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
            ..SupervisionConfig::default()
        });
        for (id, life) in lives {
            let (ctl, _io) = slot_pair(Default::default());
            sup.registry.register(ChildState {
                id: ActorId::from(*id),
                name: Some(Arc::from((*id).to_string())),
                spec: ChildSpec {
                    restart_type: RestartType::Permanent,
                    shutdown: Shutdown::default(),
                    start_timeout: None,
                    kill_grace: DEFAULT_KILL_GRACE,
                    is_supervisor: false,
                    significant: false,
                    sys: crate::actor::sys::SysOptions::default(),
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
}

#[cfg(test)]
mod task_marker_tests {
    use super::*;

    /// A task whose own poll unwinds past every catch, while it holds an
    /// abort guard: the guard drops inside that unwind and writes the panic,
    /// and the runner's reading of the join (`joined_reason`) is the plane's
    /// reason, one value with one text.
    async fn an_unwind_of_the_task_s_own_poll_reads_one_panic() {
        let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let marker = TaskMarker::default();
        let guard = AbortReason::new(status_tx.clone(), marker.clone(), ActorId::from("unwound"));
        let join = tokio::spawn(MarkedRun::new(
            marker,
            AbortRecord::new().incarnation(),
            async move {
                let _guard = guard;
                tokio::task::yield_now().await;
                if std::hint::black_box(true) {
                    panic!("the crate's own code panics in the task's poll");
                }
                StopReason::Graceful
            },
        ));
        let res = join.await;
        assert!(res.as_ref().is_err_and(|err| err.is_panic()));
        let plane = status_rx.borrow().reason.clone();
        let want = StopReason::Failure(ActorError::Panic(UNCAUGHT_PANIC.to_string()));
        assert_eq!(plane, Some(want.clone()), "the plane reads the panic");
        assert_eq!(
            crate::actor::supervision::joined_reason(res, || plane, None),
            want,
            "the runner reads the plane's reason"
        );
    }

    /// The guard of a task aborted while suspended writes `Kill`, and the
    /// runner reads `Kill`.
    async fn an_abort_of_a_suspended_task_reads_kill() {
        let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let marker = TaskMarker::default();
        let guard = AbortReason::new(status_tx.clone(), marker.clone(), ActorId::from("aborted"));
        let join = tokio::spawn(MarkedRun::new(
            marker,
            AbortRecord::new().incarnation(),
            async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                StopReason::Graceful
            },
        ));
        tokio::task::yield_now().await;
        join.abort();
        let res = join.await;
        assert!(res.as_ref().is_err_and(|err| err.is_cancelled()));
        let plane = status_rx.borrow().reason.clone();
        assert_eq!(plane, Some(StopReason::Kill));
        assert_eq!(
            crate::actor::supervision::joined_reason(res, || plane, None),
            StopReason::Kill
        );
    }
}

#[cfg(test)]
mod upper_branch_tests {
    use super::{SupervisionEvent, UpperBranch};

    impl UpperBranch {
        fn doc_name(self) -> &'static str {
            match self {
                UpperBranch::Stop => "stop lane",
                UpperBranch::System => "system channel",
                UpperBranch::Restart => "restart plane",
                UpperBranch::Death => "death plane",
                UpperBranch::Verdict => "verdict signal",
                UpperBranch::Forwarder => "forwarder plane",
                UpperBranch::Priority => "priority lane",
            }
        }
    }

    #[test]
    fn the_fast_path_clauses_follow_the_documented_reception_order() {
        let src = include_str!("runtime.rs");
        let head = "Reception order, highest first:";
        let start = src
            .find(head)
            .expect("the module docs state the reception order")
            + head.len();
        let end = start
            + src[start..]
                .find(';')
                .expect("the order ends with a semicolon");
        let listed: Vec<String> = src[start..end]
            .replace("//!", " ")
            .split(',')
            .map(|name| name.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        let mut expected: Vec<String> = UpperBranch::ORDER
            .iter()
            .map(|b| b.doc_name().to_string())
            .collect();
        expected.push("mailbox".to_string());
        assert_eq!(listed, expected);
        assert!(UpperBranch::ORDER.contains(&SupervisionEvent::Verdict.branch()));
    }
}

#[cfg(test)]
mod reply_drain_tests {
    use super::*;

    /// The turns before which the loop would hand the thread back, when turn
    /// `i` sends a reply exactly if `replies[i]` and a lane always holds
    /// messages.
    fn hand_backs(replies: &[bool]) -> Vec<usize> {
        let mut drain = ReplyDrain::new();
        let mut at = Vec::new();
        for (i, replied) in replies.iter().enumerate() {
            if drain.0 != 0 && drain.step() {
                at.push(i);
            }
            if *replied {
                drain.replied();
            }
        }
        at
    }

    fn status_info() -> ActorStatusInfo {
        ActorStatusInfo {
            id: ActorId::from("status".to_string()),
            name: None,
            status: ActorStatus::Running,
            mailbox_len: 0,
            mailbox_capacity: 1,
            priority_len: 0,
            priority_capacity: 0,
            child_count: 0,
            timer_count: 0,
            stream_count: 0,
            incarnation: crate::types::Incarnation::mint(),
            monitors: 0,
            monitored_by: 0,
            trap_exit: false,
            links: 0,
            pg_groups: 0,
            group_monitors: 0,
            aliases: 0,
            requests_in_flight: 0,
            suspended: false,
            state: None,
        }
    }

    /// What answering one status request leaves behind: whether the window
    /// opened and whether the reply is marked for the hand-back.
    fn answered(task: bool, same_thread: bool, requester_gone: bool) -> (bool, bool) {
        let (reply, requester) = oneshot::channel();
        let keep = (!requester_gone).then_some(requester);
        let mut fp_skip = 0;
        let mut drain = ReplyDrain::new();
        answer_status(
            crate::types::StatusRequest { reply, task },
            status_info(),
            same_thread,
            &mut fp_skip,
            &mut drain,
        );
        drop(keep);
        (
            fp_skip == STATUS_FAST_PATH_OFF_TURNS,
            drain.0 & ReplyDrain::REPLIED != 0,
        )
    }
}

/// The size of what every actor carries, pinned: an actor holds no telemetry
/// state of its own, so its parameter box and its run loop's future are the
/// size they are without telemetry. The sizes of a future depend on the
/// compiler and those of the standard library's types on the target, so they
/// are pinned only where they were measured, on x86_64 Linux.
#[cfg(all(test, target_arch = "x86_64", target_os = "linux"))]
#[cfg_attr(
    not(all(target_arch = "x86_64", target_os = "linux")),
    allow(dead_code, unused_imports)
)]
mod footprint_tests {
    use super::*;
    use crate::error::ActorResult;

    struct Counter {
        n: u64,
        spin_ns: u64,
    }

    enum Msg {
        Add(u64),
        Get,
    }

    impl Actor for Counter {
        type Message = Msg;
        type Response = u64;

        async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
            if let Msg::Add(k) = msg {
                self.n += k + self.spin_ns;
            }
            Ok(self.n)
        }
    }

    fn size_of_output<T>(_: impl FnOnce() -> T) -> usize {
        std::mem::size_of::<T>()
    }
}
