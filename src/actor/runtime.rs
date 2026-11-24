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

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::future::Future;
#[cfg(test)]
use std::pin::Pin;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::task::Poll;

#[cfg(test)]
use tokio::runtime::Handle;
#[cfg(test)]
use tokio::sync::oneshot;
#[cfg(test)]
use tokio::time::{Duration, Instant};

#[cfg(test)]
use crate::actor::context::ActorContext;
#[cfg(doc)]
use crate::actor::handle::ActorHandle;
#[cfg(test)]
use crate::actor::incarnation::{IncarnationRef, StatusTx, StatusValue};
#[cfg(test)]
pub(crate) use crate::actor::supervision::model::FAR_FUTURE;
use crate::actor::supervision::SupervisionConfig;
#[cfg(test)]
use crate::actor::supervision::{
    CellOutcome, ChildLifecycle, ChildLinkGuard, ChildState, DownPending, GroupPhase, GroupScope,
    Origin, RestartOutcome, SupervisionState, DEFAULT_KILL_GRACE,
};
#[cfg(test)]
use crate::actor::task_key::PollScope;
#[cfg(test)]
use crate::actor::Actor;
#[cfg(test)]
use crate::error::ActorError;
#[cfg(test)]
use crate::system::ActorSystem;
#[cfg(test)]
use crate::types::{
    ActorId, ActorStatus, ActorStatusInfo, ChildStoppedInternal, Incarnation, RestartStrategy,
    RestartType, Shutdown, StopReason,
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

/// One hook call as a turn: begins the turn on the context's clock, names the
/// turn's caller token `$token` for the catch the call site writes
/// (`|token| catch_hook_t(token, || actor.hook(..))`, so every hook call reads
/// inside its catch), awaits it, then ends the turn and queues its
/// `TurnFinished` with the hook's kind and outcome.
/// `guarded` ends and reports a turn only when it began with something armed
/// (an idle turn is neither timed nor reported); `always` ends and reports it
/// whatever it began as. Evaluates to the caught result. A macro and not a
/// function: the hook borrows the context, and the turn's record is read from
/// the context once the hook has returned.
macro_rules! hook_turn {
    ($ctx:expr, $kind:expr, guarded, |$token:ident| $catch:expr) => {{
        let turn = $ctx.clock().begin();
        let $token = $ctx.clock().caller_token(turn);
        let result = $catch.await;
        if !turn.is_idle() {
            let took = $ctx.clock().end_timed(turn);
            push_turn(
                $ctx.stop_lane().abort_record(),
                turn,
                took,
                $kind,
                HookOutcome::outcome(&result),
            );
        }
        result
    }};
    ($ctx:expr, $kind:expr, always, |$token:ident| $catch:expr) => {{
        let turn = $ctx.clock().begin();
        let $token = $ctx.clock().caller_token(turn);
        let result = $catch.await;
        let took = $ctx.clock().end_timed(turn);
        push_turn(
            $ctx.stop_lane().abort_record(),
            turn,
            took,
            $kind,
            HookOutcome::outcome(&result),
        );
        result
    }};
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
mod keep_first_tests;

#[cfg(test)]
mod teardown_adoption_tests;
// The parts of this module, in the order the generated docs list their methods.

mod spawn;
pub(crate) use self::spawn::*;

mod run_loop;
pub(crate) use self::run_loop::*;

mod end;
pub(crate) use self::end::*;

mod task;
pub(crate) use self::task::*;

mod holds;
pub(crate) use self::holds::*;

mod dispatch;
use self::dispatch::*;

mod answer;
use self::answer::*;

mod branches;
pub(crate) use self::branches::*;

mod children;
use self::children::*;

mod restart;
use self::restart::*;

mod group_stop;
pub(crate) use self::group_stop::*;

mod teardown;
use self::teardown::*;

mod final_drain;
use self::final_drain::*;
#[cfg(doc)]
use crate::error::SpawnError;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod task_marker_tests;

#[cfg(test)]
mod upper_branch_tests;

#[cfg(test)]
mod reply_drain_tests;

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
mod footprint_tests;
