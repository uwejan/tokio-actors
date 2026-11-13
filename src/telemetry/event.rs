//! The event types, their kinds and kind sets.

use std::borrow::Cow;
use std::time::Duration;

use super::actor_ref::ActorRef;
use super::subscription::SubscriptionNotice;
use crate::actor::sys::DebugEntry;
use crate::error::SpawnError;
use crate::types::{ActorId, MissPolicy, RecurringId, StopReason, SupervisionAction};

/// The kind of an event; the discriminant is the event's class bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EventKind {
    /// A stop request reached an actor's stop lane: one a caller made, or
    /// one the crate made itself (the shutdown walk, a supervisor's stop and
    /// its ladder, the link guard of a child, the cut of a start).
    StopRequested,
    /// An incarnation ended.
    ActorEnded,
    /// A supervisor decided on the death of a child.
    ChildDied,
    /// A supervisor charged a restart against its budget.
    RestartBudget,
    /// A supervisor judged a child unresponsive.
    Unresponsive,
    /// A step of a kill passed.
    KillStep,
    /// A helper task of the crate ended.
    HelperEnded,
    /// A handler was detached after it panicked.
    HandlerDetached,
    /// Events were dropped before they reached the sinks.
    Dropped,
    /// A hook of an actor finished a turn. Hot: handler-only.
    TurnFinished = 9,
    /// An actor's run of back-to-back turns ended. Hot: handler-only.
    RunFinished = 10,
    /// A recurring timer delivered a tick. Hot: handler-only.
    TimerTick = 11,
    /// A send that had to wait for mailbox room finished waiting.
    SenderWaited = 12,
    /// An actor was spawned.
    ActorSpawned = 13,
    /// A spawn was refused.
    SpawnRefused = 14,
    /// An actor finished starting.
    ActorStarted = 15,
    /// A supervisor's restart attempt reached its outcome, or ended abandoned
    /// with the supervisor.
    RestartAttempt = 16,
    /// Actors were found waiting on each other in a cycle.
    WaitCycle = 17,
    /// A turn has run past the smaller of the system monitor's `long_schedule`
    /// and the turn warning threshold, whichever of them is set.
    TurnStuck = 18,
    /// A mailbox reached the monitor's length, or fell back to the lower one
    /// (or its actor ended).
    MailboxBacklog = 19,
    /// An entry of an actor's trace. Hot: handler-only.
    Trace = 20,
}

impl EventKind {
    /// The kind with the highest discriminant.
    pub(crate) const LAST: EventKind = EventKind::Trace;

    /// The bit of this kind in a class word.
    pub(crate) const fn bit(self) -> u64 {
        1u64 << (self as u8)
    }

    /// Whether the kind is hot: raised on the path of an actor's turn, run
    /// or timer. The turn and run events of a `handle` turn that went on, and
    /// the trace entries, reach the handlers on the actor's own task between
    /// turns while the helper holds no event about that actor, and through the
    /// helper, in order, otherwise; every other turn event and a timer tick
    /// reach them through the helper.
    pub const fn is_hot(self) -> bool {
        matches!(
            self,
            Self::TurnFinished | Self::RunFinished | Self::TimerTick | Self::Trace
        )
    }
}

/// A set of [`EventKind`]s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct KindSet(u32);

/// The class bits of the hot kinds.
const HOT_BITS: u32 = (1u32 << (EventKind::TurnFinished as u8))
    | (1u32 << (EventKind::RunFinished as u8))
    | (1u32 << (EventKind::TimerTick as u8))
    | (1u32 << (EventKind::Trace as u8));

impl KindSet {
    /// Every kind, the hot ones included.
    pub const fn all() -> Self {
        Self((1u32 << (EventKind::LAST as u8 + 1)) - 1)
    }

    /// Every kind but the hot ones.
    pub const fn cold() -> Self {
        Self(Self::all().0 & !HOT_BITS)
    }

    /// The hot kinds: [`EventKind::TurnFinished`], [`EventKind::RunFinished`],
    /// [`EventKind::TimerTick`] and [`EventKind::Trace`].
    pub const fn hot() -> Self {
        Self(HOT_BITS)
    }

    /// The set of exactly `kinds`.
    pub fn of(kinds: &[EventKind]) -> Self {
        let mut bits = 0u32;
        for kind in kinds {
            bits |= 1u32 << (*kind as u8);
        }
        Self(bits)
    }

    /// Whether `kind` is in the set.
    pub fn contains(self, kind: EventKind) -> bool {
        self.0 & (1u32 << (kind as u8)) != 0
    }

    /// The kinds in either set.
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The kinds in both sets.
    pub(crate) fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// The set as class-word bits.
    pub(crate) fn bits(self) -> u64 {
        u64::from(self.0)
    }
}

/// How a stop lane answered a stop request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaiseOutcome {
    /// The request was accepted.
    Recorded,
    /// A request of strictly higher severity was already pending. (A request
    /// of equal severity is `Recorded`, and the stored reason is replaced only
    /// when the new one ranks at least as high.)
    Superseded,
    /// The lane takes no such request any more: it is sealed, open to `Kill`
    /// only, or its task is gone.
    Refused,
}

impl From<crate::types::RaiseResult> for RaiseOutcome {
    fn from(result: crate::types::RaiseResult) -> Self {
        use crate::types::RaiseResult;
        match result {
            RaiseResult::Recorded => Self::Recorded,
            RaiseResult::Superseded => Self::Superseded,
            RaiseResult::Refused => Self::Refused,
        }
    }
}

/// Where a stop request came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopOrigin {
    /// A caller asked (the actor itself among them, with `ActorContext::stop`),
    /// or the crate stopped the actor on its own path (the shutdown walk, a
    /// supervisor's stop and its ladder, the link guard of a child, the cut
    /// of a start, an exit signal).
    Requested,
    /// A signal map of the actor panicked (a monitor's, a group monitor's, a
    /// request's, or its trap map): the actor fails with that panic.
    MapPanic,
}

/// A step of the kill of an actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillStepKind {
    /// The deadline of the abort was declared.
    Declared,
    /// `Kill` was raised on the actor's stop lane.
    Raised,
    /// The abort was armed. It is reported when the system arms the backstop
    /// of the raised `Kill`, which it does while the actor is still
    /// registered: an actor that ended before that has no abort to arm, and
    /// its kill shows `Declared` and `Raised` only.
    AbortArmed,
    /// The task was aborted. It is reported once per incarnation, by the
    /// first abort that found the task unfinished, whether the reaper's at its
    /// deadline or a caller's own after the kill grace; an actor that ended
    /// before any abort reaches it shows none.
    AbortFired,
}

/// Which helper task of the crate ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperKind {
    /// Drops what arrives after a queue closed.
    LateDrain,
    /// Aborts the tasks that outlive their grace.
    Reaper,
    /// Reports the turns that run longer than the threshold and the mailboxes
    /// that cross the monitor's lengths.
    Sweep,
    /// Cuts the start of a spawn that outlives its start timeout.
    StartBound,
    /// Runs the escalation ladder of one bounded stop.
    StopLadder,
    /// Walks a system shutdown to its end.
    ShutdownWalk,
    /// Delivers queued events.
    Pump,
    /// Delivers the system monitor's reports to its actor.
    Monitor,
    /// Waits for the end of a child a `SupervisorRef` asked its supervisor
    /// to terminate, and answers the caller.
    SupervisorReply,
}

impl From<crate::helper::HelperKind> for HelperKind {
    fn from(kind: crate::helper::HelperKind) -> Self {
        use crate::helper::HelperKind as K;
        match kind {
            K::LateDrain => Self::LateDrain,
            K::Reaper => Self::Reaper,
            K::Sweep => Self::Sweep,
            K::StartBound => Self::StartBound,
            K::StopLadder => Self::StopLadder,
            K::ShutdownWalk => Self::ShutdownWalk,
            K::Pump => Self::Pump,
            K::Monitor => Self::Monitor,
            K::SupervisorReply => Self::SupervisorReply,
        }
    }
}

/// How a helper task ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelperEnd {
    /// The task ran to its end.
    Finished,
    /// A poll of the task panicked.
    Panicked,
    /// The task was dropped before its end, with `dropped` values unsettled.
    Cancelled {
        /// The number of values the task dropped unsettled.
        dropped: usize,
    },
}

impl From<crate::helper::HelperEnd> for HelperEnd {
    fn from(end: crate::helper::HelperEnd) -> Self {
        use crate::helper::HelperEnd as E;
        match end {
            E::Finished => Self::Finished,
            E::Panicked => Self::Panicked,
            E::Cancelled { dropped } => Self::Cancelled { dropped },
        }
    }
}

/// Which hook of an actor ran in a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookKind {
    /// `pre_start`.
    PreStart,
    /// `on_started`.
    OnStarted,
    /// `handle`.
    Handle,
    /// `pre_stop`.
    PreStop,
    /// `on_stopped`.
    OnStopped,
    /// `on_child_stopped`.
    OnChildStopped,
    /// A closure that runs on the actor's task at a system boundary: a
    /// closure of `inspect` or `replace_state`, the `format_status` call of a
    /// status reply or a crash report, or the factory and the configure
    /// closure of a supervisor's `start_child` through a `SupervisorRef`.
    Sys,
    /// `on_idle`.
    OnIdle,
}

/// How a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The hook returned `Ok`.
    Returned,
    /// The hook returned an error.
    Failed,
    /// The hook panicked.
    Panicked,
}

/// How a send that waited for mailbox room ended its wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// A unit of room was granted.
    Granted,
    /// The mailbox closed while the sender waited.
    Closed,
    /// The sender stopped waiting before a grant.
    Cancelled,
}

/// How a supervisor's restart attempt ended.
#[derive(Clone, Debug, PartialEq)]
pub enum AttemptOutcome {
    /// A new incarnation was adopted.
    Adopted,
    /// The attempt failed with `reason`.
    Failed {
        /// Why the attempt failed.
        reason: StopReason,
    },
    /// The attempt reached no outcome of its own: the supervisor ordered it
    /// to stop (before an incarnation existed, or while the incarnation it
    /// spawned was not yet handed over), it found its spec removed before an
    /// incarnation existed, or the supervisor ended with the attempt still in
    /// its plane and no incarnation, or one that had not ended. An
    /// incarnation that ended on its own reads, in a supervisor's final
    /// collection too, as its attempt reports it once its task ended,
    /// whether or not it is still dropping what it holds:
    /// [`Failed`](Self::Failed) when its start failed or was cut by the start
    /// timeout, [`Adopted`](Self::Adopted) when its start completed. An
    /// incarnation left running ends with the supervisor and reports its own
    /// end. When the supervisor ended, the event comes from
    /// its final collection, ahead of its end event; a supervisor that is
    /// killed has no final collection and reports the attempt from the drop
    /// of its state, which may follow its end event.
    Abandoned,
}

/// One event about the actors of a system.
#[derive(Clone, Debug)]
pub enum TelemetryEvent<'a> {
    /// A stop request reached an actor's stop lane: one a caller made, or
    /// one the crate made itself (the shutdown walk, a supervisor's stop and
    /// its ladder, the link guard of a child, the cut of a start).
    StopRequested {
        /// The actor asked to stop.
        actor: ActorRef,
        /// The reason of the request.
        reason: StopReason,
        /// How the lane answered.
        outcome: RaiseOutcome,
        /// Where the request came from.
        origin: StopOrigin,
        /// For a `Kill` raised for an exit signal, the reason the signal
        /// carries: the actor ends with it. `None` for every other request.
        cause: Option<StopReason>,
    },
    /// An incarnation ended.
    ActorEnded {
        /// The incarnation that ended.
        actor: ActorRef,
        /// Why it ended.
        reason: StopReason,
        /// How long it lived, when the lifetime was recorded.
        lifetime: Option<Duration>,
    },
    /// A supervisor decided on the death of a child.
    ChildDied {
        /// The supervisor.
        supervisor: ActorRef,
        /// The child that died.
        child: ActorRef,
        /// Why the child ended.
        reason: StopReason,
        /// What the supervisor decided.
        decision: SupervisionAction,
    },
    /// A supervisor charged a restart against its budget.
    RestartBudget {
        /// The supervisor.
        supervisor: ActorRef,
        /// The child being restarted.
        child: ActorRef,
        /// Restarts counted in the window, this one included.
        used: u32,
        /// The most restarts the window allows.
        max: u32,
        /// The length of the window.
        window: Duration,
        /// Whether the restart was within the budget.
        accepted: bool,
    },
    /// A supervisor judged a child unresponsive.
    Unresponsive {
        /// The supervisor.
        supervisor: ActorRef,
        /// The child.
        child: ActorRef,
        /// What the verdict rests on.
        cause: Cow<'a, str>,
    },
    /// A step of a kill passed.
    KillStep {
        /// The actor being killed.
        actor: ActorRef,
        /// The step.
        step: KillStepKind,
        /// For `Declared`, the window the raiser named (`None` when it used the
        /// actor's kill grace); for `AbortArmed`, the time left to the abort;
        /// `None` for the other steps.
        after: Option<Duration>,
    },
    /// A helper task of the crate ended.
    HelperEnded {
        /// Which helper.
        kind: HelperKind,
        /// The actor the helper served, if any.
        owner: Option<ActorId>,
        /// How it ended.
        end: HelperEnd,
    },
    /// A handler was detached after it panicked.
    HandlerDetached {
        /// The handler's id.
        handler: Cow<'a, str>,
        /// The panic message.
        panic: Cow<'a, str>,
    },
    /// The pump dropped `missed` events: its queue was full, its system was
    /// gone when they were raised, or they were raised inside a handler of the
    /// system. Delivered once per drain that found the count had grown; a drop
    /// that a `Dropped` handler itself causes is reported by a later drain.
    Dropped {
        /// The number of events dropped since the previous notice.
        missed: u64,
    },
    /// A hook of an actor finished a turn.
    TurnFinished {
        /// The actor.
        actor: Cow<'a, ActorRef>,
        /// The hook that ran.
        hook: HookKind,
        /// How long the turn took.
        took: Duration,
        /// How the turn ended.
        outcome: TurnOutcome,
    },
    /// A run of back-to-back turns of an actor ended.
    RunFinished {
        /// The actor.
        actor: Cow<'a, ActorRef>,
        /// The messages taken during the run.
        takes: u32,
        /// How long the run took.
        took: Duration,
    },
    /// A recurring timer delivered a tick.
    TimerTick {
        /// The actor the timer belongs to.
        actor: Cow<'a, ActorRef>,
        /// The timer.
        timer: RecurringId,
        /// The timer's miss policy.
        policy: MissPolicy,
        /// How late the tick was against its due time.
        lateness: Duration,
        /// The ticks the policy skipped.
        missed: u64,
        /// Whether a catch-up burst paused on a full mailbox.
        paused: bool,
    },
    /// A send that waited for mailbox room finished waiting. Every outcome
    /// reaches the handlers through the helper, never on the waiting task.
    SenderWaited {
        /// The actor whose mailbox the sender waited on.
        owner: ActorRef,
        /// How long the sender waited.
        waited: Duration,
        /// How the wait ended.
        outcome: WaitOutcome,
    },
    /// An actor was spawned. The event comes before the actor's task spawns
    /// and so before any event the task raises, but not before an event a
    /// concurrent stop, kill or shutdown raises for the entry once it is
    /// registered.
    ActorSpawned {
        /// The new incarnation; its first ancestor is the parent.
        actor: ActorRef,
        /// Its registered name.
        name: Option<Cow<'a, str>>,
        /// The capacity of its mailbox.
        mailbox_capacity: usize,
        /// The capacity of its priority lane, if it has one.
        priority_capacity: Option<usize>,
    },
    /// A spawn was refused: an invalid mailbox configuration, a missing
    /// runtime, a failed precheck or a registration error such as a taken
    /// name, one event per refused spawn. The refusals of a supervisor's child
    /// spawn (`DuplicateChild`, `SupervisorStopping`, `NotASupervisor`) raise
    /// none. A refused spawn that names no system is told to the sinks of the
    /// default system, if one exists.
    SpawnRefused {
        /// The name the spawn asked for.
        name: Option<Cow<'a, str>>,
        /// Why it was refused.
        error: SpawnError,
    },
    /// An actor finished starting.
    ActorStarted {
        /// The incarnation.
        actor: ActorRef,
        /// How long the start took.
        took: Duration,
    },
    /// A supervisor's restart attempt reached its outcome, or ended
    /// abandoned with the supervisor ([`AttemptOutcome::Abandoned`]).
    RestartAttempt {
        /// The supervisor.
        supervisor: ActorRef,
        /// The child being restarted.
        child: ActorRef,
        /// How the attempt ended.
        outcome: AttemptOutcome,
        /// How long the attempt took.
        took: Duration,
    },
    /// Actors were found waiting on each other in a cycle.
    WaitCycle {
        /// The actors in the cycle, starting at the caller whose wait closed
        /// it.
        members: Vec<ActorRef>,
    },
    /// A turn has run past the smaller of the system monitor's `long_schedule`
    /// and the turn warning threshold, whichever of them is set.
    TurnStuck {
        /// The actor whose turn is running.
        actor: ActorRef,
        /// How long the turn has run when it was seen.
        elapsed: Duration,
        /// The threshold it passed.
        threshold: Duration,
    },
    /// A mailbox reached the monitor's length, or fell back to the lower one
    /// (or its actor ended).
    MailboxBacklog {
        /// The actor whose mailbox it is.
        actor: ActorRef,
        /// True when the mailbox reached the length, false when it left it.
        entered: bool,
        /// The mailbox's length when it was seen.
        len: usize,
        /// The mailbox's capacity.
        capacity: usize,
    },
    /// An entry of an actor's trace.
    Trace {
        /// The actor.
        actor: Cow<'a, ActorRef>,
        /// What happened.
        entry: DebugEntry,
    },
    /// A notice to one subscription about itself.
    Notice(SubscriptionNotice),
}

impl TelemetryEvent<'_> {
    /// The kind of the event; `None` for a subscription notice.
    pub fn kind(&self) -> Option<EventKind> {
        Some(match self {
            Self::StopRequested { .. } => EventKind::StopRequested,
            Self::ActorEnded { .. } => EventKind::ActorEnded,
            Self::ChildDied { .. } => EventKind::ChildDied,
            Self::RestartBudget { .. } => EventKind::RestartBudget,
            Self::Unresponsive { .. } => EventKind::Unresponsive,
            Self::KillStep { .. } => EventKind::KillStep,
            Self::HelperEnded { .. } => EventKind::HelperEnded,
            Self::HandlerDetached { .. } => EventKind::HandlerDetached,
            Self::Dropped { .. } => EventKind::Dropped,
            Self::TurnFinished { .. } => EventKind::TurnFinished,
            Self::RunFinished { .. } => EventKind::RunFinished,
            Self::TimerTick { .. } => EventKind::TimerTick,
            Self::SenderWaited { .. } => EventKind::SenderWaited,
            Self::ActorSpawned { .. } => EventKind::ActorSpawned,
            Self::SpawnRefused { .. } => EventKind::SpawnRefused,
            Self::ActorStarted { .. } => EventKind::ActorStarted,
            Self::RestartAttempt { .. } => EventKind::RestartAttempt,
            Self::WaitCycle { .. } => EventKind::WaitCycle,
            Self::TurnStuck { .. } => EventKind::TurnStuck,
            Self::MailboxBacklog { .. } => EventKind::MailboxBacklog,
            Self::Trace { .. } => EventKind::Trace,
            Self::Notice(_) => return None,
        })
    }

    /// The actor the event is about; `None` for helper, handler, drop, refused
    /// spawn, wait cycle and notice events.
    pub fn subject(&self) -> Option<&ActorRef> {
        match self {
            Self::StopRequested { actor, .. }
            | Self::ActorEnded { actor, .. }
            | Self::KillStep { actor, .. }
            | Self::ActorSpawned { actor, .. }
            | Self::ActorStarted { actor, .. }
            | Self::TurnStuck { actor, .. }
            | Self::MailboxBacklog { actor, .. } => Some(actor),
            Self::TurnFinished { actor, .. }
            | Self::RunFinished { actor, .. }
            | Self::TimerTick { actor, .. }
            | Self::Trace { actor, .. } => Some(actor.as_ref()),
            Self::SenderWaited { owner, .. } => Some(owner),
            Self::ChildDied { child, .. }
            | Self::RestartBudget { child, .. }
            | Self::Unresponsive { child, .. }
            | Self::RestartAttempt { child, .. } => Some(child),
            Self::HelperEnded { .. }
            | Self::HandlerDetached { .. }
            | Self::Dropped { .. }
            | Self::SpawnRefused { .. }
            | Self::WaitCycle { .. }
            | Self::Notice(_) => None,
        }
    }
}

impl TelemetryEvent<'_> {
    /// The event with every borrowed field owned: what a handler keeps, or
    /// sends to another task, after its call returns.
    pub fn to_static(&self) -> TelemetryEvent<'static> {
        fn own(text: &str) -> Cow<'static, str> {
            Cow::Owned(text.to_string())
        }
        match self {
            Self::StopRequested {
                actor,
                reason,
                outcome,
                origin,
                cause,
            } => TelemetryEvent::StopRequested {
                actor: actor.clone(),
                reason: reason.clone(),
                outcome: *outcome,
                origin: *origin,
                cause: cause.clone(),
            },
            Self::ActorEnded {
                actor,
                reason,
                lifetime,
            } => TelemetryEvent::ActorEnded {
                actor: actor.clone(),
                reason: reason.clone(),
                lifetime: *lifetime,
            },
            Self::ChildDied {
                supervisor,
                child,
                reason,
                decision,
            } => TelemetryEvent::ChildDied {
                supervisor: supervisor.clone(),
                child: child.clone(),
                reason: reason.clone(),
                decision: *decision,
            },
            Self::RestartBudget {
                supervisor,
                child,
                used,
                max,
                window,
                accepted,
            } => TelemetryEvent::RestartBudget {
                supervisor: supervisor.clone(),
                child: child.clone(),
                used: *used,
                max: *max,
                window: *window,
                accepted: *accepted,
            },
            Self::Unresponsive {
                supervisor,
                child,
                cause,
            } => TelemetryEvent::Unresponsive {
                supervisor: supervisor.clone(),
                child: child.clone(),
                cause: own(cause),
            },
            Self::KillStep { actor, step, after } => TelemetryEvent::KillStep {
                actor: actor.clone(),
                step: *step,
                after: *after,
            },
            Self::HelperEnded { kind, owner, end } => TelemetryEvent::HelperEnded {
                kind: *kind,
                owner: owner.clone(),
                end: end.clone(),
            },
            Self::HandlerDetached { handler, panic } => TelemetryEvent::HandlerDetached {
                handler: own(handler),
                panic: own(panic),
            },
            Self::Dropped { missed } => TelemetryEvent::Dropped { missed: *missed },
            Self::TurnFinished {
                actor,
                hook,
                took,
                outcome,
            } => TelemetryEvent::TurnFinished {
                actor: Cow::Owned(actor.as_ref().clone()),
                hook: *hook,
                took: *took,
                outcome: *outcome,
            },
            Self::RunFinished { actor, takes, took } => TelemetryEvent::RunFinished {
                actor: Cow::Owned(actor.as_ref().clone()),
                takes: *takes,
                took: *took,
            },
            Self::TimerTick {
                actor,
                timer,
                policy,
                lateness,
                missed,
                paused,
            } => TelemetryEvent::TimerTick {
                actor: Cow::Owned(actor.as_ref().clone()),
                timer: *timer,
                policy: *policy,
                lateness: *lateness,
                missed: *missed,
                paused: *paused,
            },
            Self::SenderWaited {
                owner,
                waited,
                outcome,
            } => TelemetryEvent::SenderWaited {
                owner: owner.clone(),
                waited: *waited,
                outcome: *outcome,
            },
            Self::ActorSpawned {
                actor,
                name,
                mailbox_capacity,
                priority_capacity,
            } => TelemetryEvent::ActorSpawned {
                actor: actor.clone(),
                name: name.as_deref().map(own),
                mailbox_capacity: *mailbox_capacity,
                priority_capacity: *priority_capacity,
            },
            Self::SpawnRefused { name, error } => TelemetryEvent::SpawnRefused {
                name: name.as_deref().map(own),
                error: error.clone(),
            },
            Self::ActorStarted { actor, took } => TelemetryEvent::ActorStarted {
                actor: actor.clone(),
                took: *took,
            },
            Self::RestartAttempt {
                supervisor,
                child,
                outcome,
                took,
            } => TelemetryEvent::RestartAttempt {
                supervisor: supervisor.clone(),
                child: child.clone(),
                outcome: outcome.clone(),
                took: *took,
            },
            Self::WaitCycle { members } => TelemetryEvent::WaitCycle {
                members: members.clone(),
            },
            Self::TurnStuck {
                actor,
                elapsed,
                threshold,
            } => TelemetryEvent::TurnStuck {
                actor: actor.clone(),
                elapsed: *elapsed,
                threshold: *threshold,
            },
            Self::MailboxBacklog {
                actor,
                entered,
                len,
                capacity,
            } => TelemetryEvent::MailboxBacklog {
                actor: actor.clone(),
                entered: *entered,
                len: *len,
                capacity: *capacity,
            },
            Self::Trace { actor, entry } => TelemetryEvent::Trace {
                actor: Cow::Owned(actor.as_ref().clone()),
                entry: entry.clone(),
            },
            Self::Notice(notice) => TelemetryEvent::Notice(*notice),
        }
    }
}
