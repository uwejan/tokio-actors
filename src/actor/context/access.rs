//! What an actor reads about itself: identity, status and failure records, and `stop`, its request to end.

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;

use super::{no_extras, ActorContext, Extras, ForwarderEnd, NotClone};
use crate::actor::handle::ActorHandle;
use crate::actor::incarnation::{RaiseResult, StatusTx, StopLane, SubtreeHold, TurnClock};
use crate::actor::monitor::MonitorState;
use crate::actor::supervision::{
    AttemptTicket, DeathOutcome, Origin, RestartOutcome, SupervisionState, Verdicts,
};
use crate::actor::sys::DebugEntry;
use crate::actor::sys_debug::DebugState;
#[cfg(feature = "sys")]
use crate::actor::sys_loop::LoopSys;
use crate::actor::turn::{IdleKind, TurnState};
use crate::actor::{runtime, Actor};
use crate::error::{ActorError, StopError};
use crate::system::ActorSystem;
use crate::types::{ActorId, ActorStatus, StopReason};

impl<A: Actor> ActorContext<A> {
    // Internal constructor, called from exactly one spawn site with every
    // field already resolved; a params-object would only move the count
    // around, not reduce it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        actor_id: ActorId,
        handle: ActorHandle<A>,
        runtime: Handle,
        status_tx: StatusTx,
        system: Option<Arc<ActorSystem>>,
        name: Option<Arc<str>>,
        supervision: Option<SupervisionState>,
        turn_clock: TurnClock,
        subtree: Arc<SubtreeHold>,
    ) -> Self {
        Self {
            actor_id,
            self_handle: handle,
            runtime,
            extras: None,
            monitors: None,
            _not_clone: NotClone,
            last_error: None,
            status_tx,
            system,
            name,
            supervision: supervision.map(Box::new),
            turn_clock,
            subtree,
            task_marker: runtime::TaskMarker::default(),
        }
    }

    /// Forgets an earlier [`stop`](Self::stop): called by the run loop right
    /// before a handler runs.
    pub(crate) fn clear_self_stop(&mut self) {
        self.stop_lane().abort_record().clear_self_stop();
        if let Some(state) = self.monitors.as_deref_mut() {
            if let Some(turn) = state.turn_mut() {
                turn.end_spell();
                // A message set by a callback that does not run the chain
                // (`on_child_stopped`, a vetoing `pre_stop`) is not carried to
                // this turn.
                turn.drop_continuation();
            }
        }
    }

    /// Whether a receive timeout or an idle length is set.
    #[inline]
    pub(crate) fn idle_armed(&self) -> bool {
        self.monitors
            .as_deref()
            .is_some_and(|state| state.turn().is_some_and(TurnState::armed))
    }

    /// The nearest threshold of the current idle spell that has not fired,
    /// starting the spell when `quiet` says the loop is about to wait.
    pub(crate) fn idle_next(&mut self, now: Instant, quiet: bool) -> Option<(Instant, IdleKind)> {
        self.monitors
            .as_deref_mut()
            .and_then(|state| state.turn_mut()?.idle_next(now, quiet))
    }

    /// Records that a threshold fired in the current spell.
    pub(crate) fn mark_fired(&mut self, kind: IdleKind) {
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.mark_fired(kind);
        }
    }

    /// Marks a fire of the idle spell as running or done: the turns it causes
    /// do not end the spell.
    pub(crate) fn set_inline(&mut self, on: bool) {
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.set_inline(on);
        }
    }

    /// Makes the message of the receive timeout, inside a catch.
    pub(crate) fn make_receive_message(&self) -> Option<Result<A::Message, String>> {
        self.monitors
            .as_deref()
            .and_then(|state| state.turn()?.make_receive())
    }

    /// Whether a message waits to be handled next.
    #[inline]
    pub(crate) fn has_continuation(&self) -> bool {
        self.monitors
            .as_deref()
            .is_some_and(|state| state.turn().is_some_and(TurnState::has_continuation))
    }

    /// Takes the message to handle next.
    pub(crate) fn take_continuation(&mut self) -> Option<A::Message> {
        self.monitors
            .as_deref_mut()
            .and_then(|state| state.turn_mut()?.take_continuation())
    }

    /// Drops the message to handle next, its drop caught.
    pub(crate) fn drop_continuation(&mut self) {
        if let Some(turn) = self
            .monitors
            .as_deref_mut()
            .and_then(MonitorState::turn_mut)
        {
            turn.drop_continuation();
        }
    }

    /// The aliases this actor made that still accept messages.
    pub(crate) fn alias_count(&self) -> usize {
        self.monitors
            .as_deref()
            .map_or(0, |state| state.turn().map_or(0, TurnState::alias_count))
    }

    /// The turn state, made with the first use.
    pub(super) fn turn_mut(&mut self) -> &mut TurnState<A> {
        let owner = self.owner();
        self.monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)))
            .turn_made()
    }

    /// Whether the handler that just returned stopped its own actor; reads
    /// the flag and leaves it.
    pub(crate) fn self_stop_set(&self) -> bool {
        self.stop_lane().abort_record().self_stop_set()
    }

    /// Whether the handler that just returned stopped its own actor; reads
    /// the flag once and clears it.
    pub(crate) fn take_self_stop(&mut self) -> bool {
        self.stop_lane().abort_record().take_self_stop()
    }

    /// The extras, or an empty set when none were created.
    pub(super) fn ex(&self) -> &Extras {
        match self.extras.as_deref() {
            Some(extras) => extras,
            None => no_extras(),
        }
    }

    /// The extras, created on first use.
    pub(super) fn ex_mut(&mut self) -> &mut Extras {
        self.extras.get_or_insert_with(|| Box::new(Extras::new()))
    }

    /// Starts the incarnation's debug state, at spawn.
    pub(crate) fn set_debug(&mut self, debug: DebugState) {
        self.ex_mut().debug = Some(Box::new(debug));
    }

    /// The `sys` bundle's view of this incarnation, for serving one call.
    #[cfg(feature = "sys")]
    pub(crate) fn sys_loop(&mut self) -> LoopSys<'_> {
        let record = self.self_handle.sys_record();
        let extras = self.extras.get_or_insert_with(|| Box::new(Extras::new()));
        LoopSys::new(record, &mut extras.debug)
    }

    /// Whether any debug option is on.
    #[inline]
    pub(crate) fn debug_on(&self) -> bool {
        matches!(self.extras.as_deref(), Some(extras) if extras.debug.is_some())
    }

    /// The debug state, while any debug option is on.
    #[inline]
    pub(crate) fn debug_mut(&mut self) -> Option<&mut DebugState> {
        self.extras.as_deref_mut()?.debug.as_deref_mut()
    }

    /// The log's entries, while the log is on.
    pub(crate) fn debug_ring(&self) -> Option<&[DebugEntry]> {
        self.extras
            .as_deref()?
            .debug
            .as_deref()
            .and_then(DebugState::ring_entries)
    }

    /// The marker of this actor's task's own poll (see
    /// [`runtime::TaskMarker`]).
    pub(crate) fn task_marker(&self) -> runtime::TaskMarker {
        self.task_marker.clone()
    }

    /// A handle to the actor's turn clock.
    pub(crate) fn turn_clock(&self) -> TurnClock {
        self.turn_clock.clone()
    }

    /// The incarnation's turn clock, borrowed.
    pub(crate) fn clock(&self) -> &TurnClock {
        &self.turn_clock
    }

    /// Sets a label for this actor, shown in a system snapshot.
    pub fn set_label(&self, label: impl Into<String>) {
        crate::telemetry::set_label(
            self.stop_lane().abort_record(),
            self.system_in_hand(),
            &self.actor_id,
            Arc::from(label.into()),
        );
    }

    // - Identity & status --------------------------------------------------

    /// Returns the unique identifier of this actor.
    pub fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    /// This incarnation as the owner of the drops its log lines name: its
    /// id, with the sensitivity its record carries.
    pub(crate) fn owner(&self) -> runtime::Owner {
        runtime::Owner::of(&self.actor_id, self.stop_lane().abort_record())
    }

    /// Returns the registered name of this actor, if any.
    pub fn actor_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Returns a handle to this actor.
    ///
    /// This handle, and any internal self-reference derived from it (a
    /// recurring [`schedule`](Self::schedule) or an attached
    /// [`add_stream`](Self::add_stream) forwarder both hold a clone
    /// internally), never extends the actor's lifetime: process lifetime
    /// governs timer lifetime, not the reverse. An actor still runs until it
    /// is stopped, killed, or its system shuts down, whether or not any
    /// external handle remains; when it stops, every internal self-reference
    /// is torn down with it. Parity: Erlang/OTP pid-addressed timers are
    /// automatically canceled when their target process dies (erlang.org,
    /// OTP 28, `erlang:start_timer/4`).
    pub fn self_handle(&self) -> ActorHandle<A> {
        self.self_handle.clone()
    }

    /// The actor's own handle, borrowed, for a read that needs no clone of it.
    pub(crate) fn self_handle_ref(&self) -> &ActorHandle<A> {
        &self.self_handle
    }

    /// Stops this actor with `reason`, from inside one of its callbacks.
    ///
    /// The request goes onto the actor's own stop lane exactly as
    /// [`ActorHandle::stop`](crate::actor::handle::ActorHandle::stop) writes
    /// it, so one reason gets one outcome by either route: a
    /// [`Graceful`](StopReason::Graceful) or [`Shutdown`](StopReason::Shutdown)
    /// request passes the actor's own `pre_stop`, which may veto it, a pending
    /// request of a stronger reason (a parent's
    /// [`ParentRequest`](StopReason::ParentRequest), a failure) is never
    /// replaced by this one, and the actor ends after the callback returns,
    /// ahead of any queued message.
    ///
    /// When called from `handle` while a `send` is waiting for its reply, the
    /// reply the handler returns is held and goes out once the actor's
    /// `on_stopped` has returned and its name is free, before its reason is
    /// written, as the reply of `{stop, Reason, Reply, State}` follows the
    /// `terminate/2` call in an Erlang/OTP `gen_server`. If the stop is
    /// vetoed by `pre_stop`, the actor keeps running and the reply is sent at
    /// once. A `Kill` that ends the actor first, or an abort, gives the caller
    /// `AskError::ResponseDropped` with `Kill`.
    ///
    /// The reply is sent after the actor's children have stopped, as Erlang/OTP
    /// replies after `terminate/2`, which for a supervisor stops its children
    /// first. A child that awaits this reply inside its own callback therefore
    /// waits within its own `Shutdown` bound before it is stopped, and without
    /// a bound under `Shutdown::Infinity`, as in OTP.
    ///
    /// Erlang/OTP: `{stop, Reason, State}` and `{stop, Reason, Reply, State}`
    /// returned from a callback; `StopReason::Shutdown` is the
    /// `{shutdown, Term}` reason.
    ///
    /// # Errors
    /// - [`StopError::KillRefused`] if `reason` is [`StopReason::Kill`]; a
    ///   `Kill` is raised through a handle or the system, which arm its
    ///   backstop. Nothing is written.
    /// - [`StopError::Closed`] if the stop lane takes no such request any
    ///   more: the actor's task has ended or the lane is sealed. A stop made
    ///   from `on_stopped` returns `Ok` and changes nothing, since the lane
    ///   narrows only after the hook returns. Nothing is written on an
    ///   error.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_actors::{
    ///     actor::{Actor, ActorExt, context::ActorContext},
    ///     ActorResult, StopReason,
    /// };
    ///
    /// struct Job;
    ///
    /// enum Msg {
    ///     Finish,
    /// }
    ///
    /// impl Actor for Job {
    ///     type Message = Msg;
    ///     type Response = &'static str;
    ///
    ///     async fn handle(
    ///         &mut self,
    ///         msg: Msg,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<&'static str> {
    ///         match msg {
    ///             Msg::Finish => {
    ///                 ctx.stop(StopReason::Shutdown("finished".into()))
    ///                     .expect("the message loop is running");
    ///                 Ok("done")
    ///             }
    ///         }
    ///     }
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let job = Job.spawn().await.unwrap();
    /// assert_eq!(job.send(Msg::Finish).await.unwrap(), "done");
    /// assert_eq!(
    ///     job.wait_stopped().await,
    ///     StopReason::Shutdown("finished".into())
    /// );
    /// # }
    /// ```
    pub fn stop(&mut self, reason: StopReason) -> Result<(), StopError> {
        if matches!(reason, StopReason::Kill) {
            return Err(StopError::KillRefused);
        }
        // The actor's own stop, never a `Kill`: refused above, so no
        // deadline is owed and no abort is armed.
        let raised = crate::system::raise_noted(
            self.system_in_hand(),
            self.stop_lane(),
            reason,
            crate::telemetry::StopOrigin::Requested,
        );
        if raised == RaiseResult::Refused {
            return Err(StopError::Closed);
        }
        self.stop_lane().abort_record().mark_self_stop();
        Ok(())
    }

    /// Asks that this start be ignored, as Erlang/OTP's `init/1` returning
    /// `ignore`: read once, when `pre_start` returns `Ok(())`, after which the
    /// actor runs no `on_started`, no message loop and no `on_stopped` and
    /// ends with `StopReason::Graceful`; a call anywhere else, or a
    /// `pre_start` that returns `Err` or panics, leaves it without effect.
    ///
    /// A root spawn of an actor whose start was ignored answers
    /// [`SpawnError::Ignored`](crate::SpawnError::Ignored). A `Kill` that is
    /// pending when `pre_start` returns ends the actor as `Kill` and the
    /// request is never read.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio_actors::{
    ///     actor::{Actor, ActorExt, context::ActorContext},
    ///     ActorResult, SpawnError,
    /// };
    ///
    /// struct Optional;
    ///
    /// impl Actor for Optional {
    ///     type Message = ();
    ///     type Response = ();
    ///
    ///     async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         // Nothing to do here, so the actor is not started.
    ///         ctx.ignore();
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// assert!(matches!(
    ///     Optional.spawn().await,
    ///     Err(SpawnError::Ignored)
    /// ));
    /// # }
    /// ```
    pub fn ignore(&mut self) {
        self.ex_mut().ignore_start = true;
    }

    /// Reads and clears the request of [`ignore`](Self::ignore), creating no
    /// extras for an actor that never made one.
    pub(crate) fn take_ignore(&mut self) -> bool {
        self.extras
            .as_deref_mut()
            .is_some_and(|e| std::mem::take(&mut e.ignore_start))
    }

    /// Ends the supervisor's start phase, in which a strict start keeps the
    /// acknowledgment of each child spawned. Tells whether the phase was on.
    pub(crate) fn end_init_phase(&mut self) -> bool {
        self.supervision
            .as_deref_mut()
            .is_some_and(|sup| std::mem::take(&mut sup.init_phase))
    }

    /// The actor's own stop lane, borrowed from its handle: the run loop reads
    /// and narrows it at its start and its end without cloning the handle.
    pub(crate) fn stop_lane(&self) -> &StopLane {
        self.self_handle.stop_lane_ref()
    }

    /// The identity fields of this actor's log statements.
    #[cfg(feature = "tracing")]
    pub(crate) fn log_fields(&self) -> crate::telemetry::LogFields<'static> {
        crate::telemetry::log_fields(self.stop_lane().abort_record(), self.system_in_hand())
    }

    /// Records a failure that occurred during message processing.
    pub fn record_failure(&mut self, error: ActorError) {
        self.last_error = Some(error);
    }

    /// Returns the last error recorded by this actor, if any.
    pub fn last_error(&self) -> Option<&ActorError> {
        self.last_error.as_ref()
    }

    /// Returns the most recent panic from a poll of a timer or stream
    /// forwarder task, if any. Such a panic never stops this actor - the
    /// message loop keeps running - so this is the only way to observe one
    /// after the fact.
    pub fn last_forwarder_error(&self) -> Option<&ActorError> {
        self.ex().last_forwarder_error.as_ref()
    }

    /// Returns the most recent panic of a task the crate runs to supervise
    /// this actor's children (the task that stops a child or restarts it), if
    /// any. Such a panic never stops this actor and is never recorded by
    /// [`last_error`](Self::last_error); the child's own end is still
    /// reported through `on_child_stopped` with the reason it ended with.
    pub fn last_supervision_task_error(&self) -> Option<&ActorError> {
        self.ex().last_supervision_task_error.as_ref()
    }

    /// Accounts for a death-plane task that failed instead of completing,
    /// and records its panic, if it panicked. Returns the id of the
    /// replacement task when the incarnation it follows had ended already.
    pub(crate) fn lost_death_task(&mut self, err: JoinError) -> Option<tokio::task::Id> {
        let (panic, replacement) = match self.supervision.as_deref_mut() {
            Some(sup) => sup.lost_death_task(err),
            None => (None, None),
        };
        if let Some(panic) = panic {
            self.ex_mut().last_supervision_task_error = Some(panic);
        }
        replacement
    }

    /// Accounts for a restart-plane task that failed instead of completing,
    /// and records its panic, if it panicked. Returns the outcome to route
    /// when the attempt failed before it spawned an incarnation, and the id
    /// of the replacement task when the incarnation it follows had ended
    /// already.
    pub(crate) fn lost_restart_task(
        &mut self,
        err: JoinError,
    ) -> (Option<RestartOutcome>, Option<tokio::task::Id>) {
        let (outcome, panic, replacement) = match self.supervision.as_deref_mut() {
            Some(sup) => sup.lost_restart_task(err),
            None => (None, None, None),
        };
        if let Some(panic) = panic {
            self.ex_mut().last_supervision_task_error = Some(panic);
        }
        (outcome, replacement)
    }

    /// Returns the current lifecycle status of the actor.
    pub fn status(&self) -> ActorStatus {
        self.status_tx.status()
    }

    pub(crate) fn set_status(&mut self, status: ActorStatus) {
        // This write must succeed even if every `ActorHandle` (and therefore
        // every reader of the plane) has already been dropped. The plane's
        // reason is never touched here.
        self.status_tx.send_if_modified(|value| {
            value.status = status;
            true
        });
    }

    // - Supervision --------------------------------------------------------

    /// Starts a restart attempt of the child `child_id` as the incarnation
    /// `seq` (see [`SupervisionState::initiate`]) and, when the supervisor's
    /// system counts attempts, keeps the attempt's ticket until its outcome
    /// is taken. Returns whether the attempt started.
    pub(crate) fn initiate_attempt(
        &mut self,
        child_id: &ActorId,
        seq: u64,
        origin: Origin,
    ) -> bool {
        let ticket = AttemptTicket::begin(
            self.self_handle.stop_lane_ref().abort_record(),
            self.system.as_deref(),
            &self.actor_id,
        );
        let Some(sup) = self.supervision.as_deref_mut() else {
            return false;
        };
        let started = sup.initiate(child_id, seq, origin);
        match (started, ticket) {
            (true, Some(ticket)) => sup.note_attempt(child_id, seq, ticket),
            // A refused start is no attempt: its count falls with no event.
            (false, Some(ticket)) => ticket.discard(),
            (_, None) => {}
        }
        started
    }

    pub(crate) fn supervision_mut(&mut self) -> Option<&mut SupervisionState> {
        self.supervision.as_deref_mut()
    }

    pub(crate) fn supervision_ref(&self) -> Option<&SupervisionState> {
        self.supervision.as_deref()
    }

    /// Whether a final delivery of `on_child_stopped` is running: the
    /// supervisor is stopping, and its supervision calls are refused.
    pub(super) fn supervisor_stopping(&self) -> bool {
        self.supervision
            .as_deref()
            .is_some_and(|sup| sup.final_delivery)
    }

    /// Takes the supervision state out of the context, leaving `None`
    /// behind. Used only by the run loop's Kill cascade-inversion path:
    /// dropping the returned value directly aborts every live child's
    /// runner task without this actor ever awaiting any of them (see the
    /// `runtime` module docs).
    pub(crate) fn take_supervision(&mut self) -> Option<SupervisionState> {
        self.supervision.take().map(|b| *b)
    }

    /// Borrows the supervisor's restart and death planes (if this actor is a
    /// supervisor) together with the forwarder plane every actor has,
    /// disjointly enough for the run loop's combined `select!` to poll all
    /// three in one branch. One accessor, not three, because that branch's
    /// future must hold a single `&mut ActorContext` borrow for its whole
    /// span - see the run loop's own docs.
    #[allow(clippy::type_complexity)]
    pub(crate) fn supervision_planes_mut(
        &mut self,
    ) -> (
        Option<(
            &mut JoinSet<RestartOutcome>,
            &mut JoinSet<DeathOutcome>,
            &Arc<Verdicts>,
        )>,
        Option<&mut JoinSet<ForwarderEnd>>,
    ) {
        let planes = self
            .supervision
            .as_mut()
            .map(|sup| (&mut sup.restart_set, &mut sup.death_set, &sup.verdicts));
        (
            planes,
            self.extras
                .as_deref_mut()
                .map(|extras| &mut extras.forwarders),
        )
    }
}
