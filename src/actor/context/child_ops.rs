//! Managing children from a callback or at a turn boundary: stop, terminate, restart and delete, awaited or begun.

use super::{manual_reply, ActorContext, Committed, JoinTicket};
use crate::actor::supervision::{
    await_cell, await_subtree, manual_commit, rules, CellOutcome, ChildLifecycle, ChildState,
    CommitEffect, ManualOp, ManualStop, Origin, RestartFn,
};
use crate::actor::Actor;
#[cfg(doc)]
use crate::error::SpawnError;
use crate::error::SupervisionError;
#[cfg(doc)]
use crate::types::Shutdown;
use crate::types::{ActorId, ChildEvent, RestartStrategy, StopChildOutcome, StopReason};

impl<A: Actor> ActorContext<A> {
    /// Manually stops a supervised child and lets its policy restart it
    /// (a "bounce"): a Permanent child is restarted BUDGET-FREE ("a manual
    /// stop is not a failure"), Transient and Temporary children stay down,
    /// and a SimpleOneForOne child's dynamic spec is removed entirely. A
    /// child that turns out to have crashed (`Failure`) rather than obeyed
    /// the stop signal is NOT treated as a manual bounce: it is charged to
    /// the restart budget and evaluated by the ordinary strategy instead,
    /// exactly like any other crash.
    ///
    /// Honors the child's [`Shutdown`] policy: a vetoing or slow child is
    /// escalated to Kill at the timeout, with a task-abort backstop behind
    /// it. When the outcome is [`StopChildOutcome::Stopped`] the call returns
    /// once the child's fate cell is populated - which tokio guarantees only
    /// happens after every one of the child's own drops, its registry entry
    /// included, have already run - so by the time this resolves the child is
    /// fully gone and its name is free (`Ok` if it was already stopped and
    /// its death is decided, see the errors below). An
    /// [`StopChildOutcome::Absorbed`] call returns at once: the pending group
    /// restart still owns the child, which is alive or about to be restarted
    /// by that group, and its name is not free.
    ///
    /// A supervisor-child's stop recursively waits for that subtree's own
    /// teardown: the worst case is the sum of every level's [`Shutdown`]
    /// budget below it, and a [`Shutdown::Infinity`] descendant anywhere in
    /// that subtree makes the wait unbounded (accepted; escape via the
    /// escalation ladder on every non-`Infinity` policy in the chain). That
    /// wait covers the child's own orderly teardown of its children; it does
    /// not wait for actors of the subtree that outlive a `Kill` or an abort,
    /// which [`terminate_child`](Self::terminate_child) also waits for.
    ///
    /// A Permanent child's restart begins only after the child's previous
    /// incarnation and every actor it spawned have ended. That wait is bounded
    /// by the longest chain of stop budgets in the subtree (a worker's kill
    /// grace, a supervisor child's `Shutdown` window plus its kill grace, and
    /// for a worker that has called `trap_exit` its own `Shutdown` window as
    /// well), each followed by a short settle time, plus one more settle time,
    /// and unbounded only while a supervisor child under [`Shutdown::Infinity`]
    /// runs in it, or a worker under `Infinity` that traps exit signals, or a
    /// budget is too large to represent. Past that bound, the wait gives no
    /// verdict on an actor before the abort the system owes it has had the
    /// settle time to end it: an abort that is late holds the wait, for at most
    /// the bound once more plus one more settle time. If the subtree does not
    /// end within that wait, no new incarnation starts and the supervisor stops
    /// with [`SupervisionError::ChildUnresponsive`], so that its own supervisor
    /// decides.
    ///
    /// To stop a child WITHOUT any restart, use
    /// [`terminate_child`](Self::terminate_child).
    ///
    /// # Outcome
    /// [`StopChildOutcome::Stopped`] when the call completed a stop, and
    /// [`StopChildOutcome::Absorbed`] when the child is a `Permanent` member
    /// of a group restart that is still pending and has still to restart it:
    /// the call then changed nothing, the group stops the child, if it has
    /// not already, and restarts it. The members a pending group restart
    /// covers follow their restart type: a `Permanent` member is absorbed
    /// while the group has yet to restart it, also when it restarted the
    /// child once already and is restarting the whole group again, and is
    /// refused once the group restarted it for good or released it; a
    /// `Transient` or `Temporary` member, and such a child started while the
    /// group restart is pending, is stopped as
    /// [`terminate_child`](Self::terminate_child) stops it: it stays down
    /// through that group restart, a `Transient` child keeping its
    /// specification and a `Temporary` child removed.
    ///
    /// Cancel safety: the escalation belongs to the child's own runner.
    /// Abandoning this future at any point, for example with
    /// `tokio::time::timeout`, still stops the child under its `Shutdown`
    /// policy. Calling it again while that stop is in progress waits for the
    /// same stop, or returns at once where a pending group restart covers the
    /// child (`StopChildOutcome::Absorbed` for a `Permanent` member that
    /// restart has still to restart, `ChildRestarting` once it has restarted it
    /// for good or released it). A child that then outlasts its stop carries
    /// the Unresponsive verdict with no caller left to report it, and the
    /// supervisor stops with [`SupervisionError::ChildUnresponsive`] for it.
    ///
    /// To force a restart that charges the restart budget, kill the child:
    /// [`ActorSystem::kill_by_id`](crate::system::ActorSystem::kill_by_id),
    /// or [`stop_within`](crate::actor::handle::ActorHandle::stop_within)
    /// with [`StopReason::Kill`] on its handle. Its supervisor restarts it per
    /// its restart type, as Erlang/OTP does for `exit(Child, kill)`.
    ///
    /// # Errors
    /// - [`SupervisionError::NotASupervisor`] / [`SupervisionError::ChildNotFound`]
    /// - [`SupervisionError::ChildRestarting`] while the child has a restart
    ///   attempt in flight; while it has died and its death awaits its
    ///   restart decision (a crash a bounce met, or a death the supervisor
    ///   has not handled yet): the decision is made once, by the supervisor,
    ///   and a bounce neither makes it nor replaces it (OTP's
    ///   `terminate_child` flushes the exit and answers `ok` here; so does
    ///   [`terminate_child`](Self::terminate_child), which dismisses the
    ///   decision); for a `Permanent` child that a pending group restart
    ///   already restarted or released with
    ///   [`terminate_child`](Self::terminate_child), until that restart
    ///   completes; and for the member of a pending group restart whose own
    ///   restart attempt is in flight.
    /// - [`SupervisionError::ChildUnresponsive`] if the child survives even
    ///   the abort backstop (a non-yielding handler; see the crate docs).
    /// - [`SupervisionError::SupervisorStopping`] when called inside
    ///   [`on_child_stopped`](crate::actor::Actor::on_child_stopped) while the
    ///   supervisor itself is stopping.
    pub async fn stop_child(
        &mut self,
        child: impl Into<ActorId>,
    ) -> Result<StopChildOutcome, SupervisionError> {
        self.manual_stop_child(child.into(), ManualStop::Bounce)
            .await
    }

    /// Stops a supervised child WITHOUT restarting it - the OTP
    /// `terminate_child/2` equivalent. The child spec is kept (visible in
    /// [`children`](Self::children) with `is_alive == false`) so the child can
    /// later be revived with [`restart_child`](Self::restart_child) or removed
    /// with [`delete_child`](Self::delete_child); a Temporary child's spec is
    /// deleted immediately (OTP parity), as is a SimpleOneForOne child's. A
    /// crash that lands while this call is stopping the child (the child dies
    /// of `Failure` at roughly the same moment) is absorbed into this call's
    /// own completion rather than being reported as an independent failure
    /// (OTP unlink-and-flush parity).
    ///
    /// Succeeds for any known child: one already down, one whose restart
    /// decision is still owed (a crash a bounce met, or a death the
    /// supervisor has not handled yet) included. It dismisses an owed
    /// decision, so the child is not restarted and its death is reported once
    /// to [`on_child_stopped`](crate::actor::Actor::on_child_stopped) as
    /// [`SupervisionAction::Removed`](crate::SupervisionAction::Removed),
    /// cancels a pending restart, and returns once the child and every actor
    /// it spawned have ended. A second `terminate_child` of a child whose
    /// stop is already in progress waits for that stop, and one that finds a
    /// `stop_child` in progress turns it into a terminate: the child stays
    /// down.
    ///
    /// Returns only once the child is fully gone and its registered name is
    /// free. Where the spec is kept, [`delete_child`](Self::delete_child)
    /// succeeds afterward, even from the very same handler invocation that
    /// awaited this call, and a same-name [`spawn_child`](Self::spawn_child)
    /// made after the `delete_child` succeeds; made before it, `spawn_child`
    /// is refused with [`SpawnError::DuplicateChild`] (OTP `already_present`).
    /// Where the call removes the spec (a `Temporary` child, or a child of a
    /// `SimpleOneForOne` supervisor), a same-name `spawn_child` succeeds at
    /// once and `delete_child` answers [`SupervisionError::ChildNotFound`].
    ///
    /// Unlike [`stop_child`](Self::stop_child), this call also waits for the
    /// child's subtree: it returns once the child and every actor it spawned
    /// have ended, a child already stopped whose actors are still ending
    /// included. The wait is bounded by the longest chain of stop budgets in
    /// the subtree (a worker's kill grace, a supervisor child's `Shutdown`
    /// window plus its kill grace, and for a worker that has called `trap_exit`
    /// its own `Shutdown` window as well), each followed by a short settle
    /// time, plus one more settle time, and unbounded only while a supervisor
    /// child under [`Shutdown::Infinity`] runs in it, or a worker under
    /// `Infinity` that traps exit signals, or a budget is too large to
    /// represent. Past that bound, the wait gives no verdict on an actor before
    /// the abort the system owes it has had the settle time to end it: an abort
    /// that is late holds the wait, for at most the bound once more plus one
    /// more settle time. If an actor never yields, the call returns
    /// [`SupervisionError::ChildUnresponsive`] and the child stays stopped;
    /// once its spec is gone, a [`spawn_child`](Self::spawn_child) under the
    /// same id waits for that subtree again.
    ///
    /// While a group restart is pending, a `terminate_child` keeps the child
    /// down through every retry of that group restart, a child started since
    /// included. A group restart stops its members one at a time; a
    /// `terminate_child` of a member the group has not reached yet stops that
    /// member at once, beside the member the group is stopping, and returns
    /// once it ended. A member under [`Shutdown::Infinity`] whose stop is in
    /// progress and that vetoed it is asked once more by the call, an ask the
    /// group restart then does not repeat.
    ///
    /// Blocking and escalation (including the recursive subtree-teardown
    /// latency note) match [`stop_child`](Self::stop_child). A child configured
    /// with [`Shutdown::Infinity`] makes this call wait without bound:
    /// Erlang/OTP asks for `infinity` on a child that is itself a supervisor,
    /// whose own subtree can legitimately take an unbounded time to unwind, and
    /// allows it on a worker with a warning. This crate accepts it on any
    /// child, and a leaf actor configured with `Infinity` is a risk it does not
    /// guard against. A child still running its first initialization is stopped
    /// bounded by its `Shutdown` from the call; one whose initialization waits
    /// on this supervisor deadlocks this call unless that bound ends it (under
    /// `Infinity` nothing does: the hazard `pre_stop` already has inside a
    /// handler).
    ///
    /// Cancel safety: the escalation belongs to the child's own runner.
    /// Abandoning this future at any point, for example with
    /// `tokio::time::timeout`, still stops the child under its `Shutdown`
    /// policy, and calling it again while that stop is in progress waits for
    /// the same stop. The caller may bound its own wait and drop the future
    /// safely; a child that then outlasts its stop carries the Unresponsive
    /// verdict with no caller left to report it, and the supervisor stops with
    /// [`SupervisionError::ChildUnresponsive`] for it.
    ///
    /// # Errors
    /// - [`SupervisionError::NotASupervisor`] / [`SupervisionError::ChildNotFound`]
    /// - [`SupervisionError::ChildRestarting`] only while a restart attempt
    ///   is in flight, the front of a group restart's chain included.
    /// - [`SupervisionError::ChildUnresponsive`] if the child outlasts the
    ///   abort backstop, or its subtree outlasts the stop windows (the child
    ///   is left down).
    /// - [`SupervisionError::SupervisorStopping`] inside a final delivery,
    ///   that is when called inside
    ///   [`on_child_stopped`](crate::actor::Actor::on_child_stopped) while the
    ///   supervisor itself is stopping. An owed death is dismissed, not
    ///   refused.
    pub async fn terminate_child(
        &mut self,
        child: impl Into<ActorId>,
    ) -> Result<(), SupervisionError> {
        self.manual_stop_child(child.into(), ManualStop::Terminate)
            .await
            .map(|_| ())
    }

    /// Revives a child previously stopped with
    /// [`terminate_child`](Self::terminate_child) - the OTP `restart_child/2`
    /// equivalent. Initiates the restart from the stored child spec and
    /// returns; completion is observable via a name lookup or
    /// [`children`](Self::children). The restart begins only after the child's
    /// previous incarnation and every actor it spawned have ended, waiting
    /// without blocking the supervisor. The wait is bounded by the longest
    /// chain of stop budgets in the subtree (a worker's kill grace, a
    /// supervisor child's `Shutdown` window plus its kill grace, and for a
    /// worker that has called `trap_exit` its own `Shutdown` window as well),
    /// each followed by a short settle time, plus one more settle time, and
    /// unbounded only while a supervisor child under [`Shutdown::Infinity`]
    /// runs in it, or a worker under `Infinity` that traps exit signals, or a
    /// budget is too large to represent. Past that bound, the wait gives no
    /// verdict on an actor before the abort the system owes it has had the
    /// settle time to end it: an abort that is late holds the wait, for at most
    /// the bound once more plus one more settle time. If the subtree does not
    /// end within that wait, no new incarnation starts and the supervisor stops
    /// with [`SupervisionError::ChildUnresponsive`], so that its own supervisor
    /// decides.
    ///
    /// The call returns before the child initializes. If initialization then
    /// fails, `on_child_stopped` receives [`SupervisionAction::Removed`](crate::SupervisionAction::Removed) with
    /// the initialization error, the spec is kept, no restart budget is
    /// charged and no sibling is touched - unless a pending group restart has
    /// committed the child, which then restarts it in its slot.
    ///
    /// To force a restart of a running child, kill it (see
    /// [`stop_child`](Self::stop_child)): its supervisor restarts it per its
    /// restart type and charges the restart budget.
    ///
    /// # Errors
    /// - [`SupervisionError::NotASupervisor`] / [`SupervisionError::ChildNotFound`]
    /// - [`SupervisionError::ChildRunning`] if the child's task is running.
    /// - [`SupervisionError::ChildRestarting`] if a restart attempt is in
    ///   flight; if the child's stop is in progress, whether or not its task
    ///   has ended; if its death awaits its restart decision (a child whose
    ///   task has just ended included); or if it belongs to a pending group
    ///   restart, a member released by
    ///   [`terminate_child`](Self::terminate_child) included, until that
    ///   restart completes. A running child that group restart has already
    ///   restarted is refused the same way, whatever its membership.
    /// - [`SupervisionError::SupervisorStopping`] when called inside
    ///   [`on_child_stopped`](crate::actor::Actor::on_child_stopped) while the
    ///   supervisor itself is stopping.
    pub fn restart_child(&mut self, child: impl Into<ActorId>) -> Result<(), SupervisionError> {
        if self.supervisor_stopping() {
            return Err(SupervisionError::SupervisorStopping);
        }
        let id = child.into();
        let seq = self.resolved_system().next_incarnation();
        let sup = self
            .supervision
            .as_mut()
            .ok_or(SupervisionError::NotASupervisor)?;
        manual_reply(sup.manual_answer(ManualOp::RestartChild, &id), &id)?;
        self.initiate_attempt(&id, seq, Origin::Manual);
        Ok(())
    }

    /// Removes a stopped child's spec - the OTP `delete_child/2` equivalent.
    /// The child must not be running or restarting; use
    /// [`terminate_child`](Self::terminate_child) first.
    ///
    /// A [`spawn_child`](Self::spawn_child) under the same id waits, inside the
    /// calling handler, until every actor the deleted child spawned has ended,
    /// and fails with [`SpawnError::ChildUnresponsive`] if one of them never
    /// yields. The wait is bounded by the longest chain of stop budgets in the
    /// subtree (a worker's kill grace, a supervisor child's `Shutdown` window
    /// plus its kill grace, and for a worker that has called `trap_exit` its
    /// own `Shutdown` window as well), each followed by a short settle time,
    /// plus one more settle time, and unbounded only while a supervisor child
    /// under [`Shutdown::Infinity`] runs in it, or a worker under `Infinity`
    /// that traps exit signals, or a budget is too large to represent. Past
    /// that bound, the wait gives no verdict on an actor before the abort the
    /// system owes it has had the settle time to end it: an abort that is late
    /// holds the wait, for at most the bound once more plus one more settle
    /// time.
    ///
    /// # Errors
    /// The same set as [`restart_child`](Self::restart_child), except that a
    /// member released by [`terminate_child`](Self::terminate_child) from a
    /// pending group restart is answered by its own state: `Ok` once it is
    /// down, [`SupervisionError::ChildRestarting`] while its stop is in
    /// progress, [`SupervisionError::ChildRunning`] while it runs.
    pub fn delete_child(&mut self, child: impl Into<ActorId>) -> Result<(), SupervisionError> {
        if self.supervisor_stopping() {
            return Err(SupervisionError::SupervisorStopping);
        }
        let id = child.into();
        let sup = self
            .supervision
            .as_mut()
            .ok_or(SupervisionError::NotASupervisor)?;
        manual_reply(sup.manual_answer(ManualOp::DeleteChild, &id), &id)?;
        let factory = sup.remove_spec(&id);
        // The factory is user code: dropped as the call's last step, a panic
        // in its drop is a panic of the callback that called it.
        drop(factory);
        Ok(())
    }

    /// Shared machinery for stop_child / terminate_child.
    ///
    /// The decision of the call is `rules::manual_commit`; this executes it.
    /// The commit is synchronous: the ledger moves to `Stopping{kind}`, the
    /// first signal is raised and the stop order is written to the child's
    /// runner with ZERO awaits between them, so a caller that drops this
    /// future immediately after issuing it has still made the stop happen.
    /// From there the child's own runner escalates the stop under its
    /// `Shutdown` policy (Kill, then abort) and records the end; this call
    /// only waits on the runner's cell (populated only after every one of the
    /// child's drops, its registry entry included, have already run) and
    /// classifies the result. Dropping this future at any await changes
    /// nothing about how the child is stopped, and the run loop completes the
    /// classification when the child's death arrives. A second call on a
    /// child whose manual stop is in progress waits for that same stop.
    ///
    /// A member of an in-flight `OneForAll`/`RestForOne` group is answered by
    /// its membership: `stop_child` of a `Permanent` member is absorbed or
    /// refused, and `terminate_child` records that the group must leave the
    /// child down and, when the group awaits the child's end, waits for it. A
    /// member the group's stop queue has not reached has no stop in progress,
    /// so the call orders it itself; a member whose stop is in progress is
    /// joined, never given a second ladder, and a vetoing `Infinity` member
    /// is asked once more when nobody asked it since.
    async fn manual_stop_child(
        &mut self,
        id: ActorId,
        kind: ManualStop,
    ) -> Result<StopChildOutcome, SupervisionError> {
        let (outcome, effective) = self.manual_stop_decided(id.clone(), kind).await?;
        // A `stop_child` handled as the child's `terminate_child` waits for
        // the child's subtree too.
        if effective == ManualStop::Terminate {
            self.await_child_subtree(&id).await?;
        }
        Ok(outcome)
    }

    /// Stops a supervised child exactly as [`stop_child`](Self::stop_child)
    /// commits the stop, and returns at once: the commit without the wait
    /// for the child's end.
    ///
    /// The supervisor's record of the child moves, the first signal is raised
    /// and the stop order is written before this call returns, and the child's
    /// own runner escalates the stop under its `Shutdown` policy from there.
    /// The supervisor's death arm classifies the end when it arrives and reports
    /// it once to
    /// [`on_child_stopped`](crate::actor::Actor::on_child_stopped), as it
    /// does for a caller that dropped its `stop_child` future after the
    /// commit. The returned outcome is the one `stop_child` answers:
    /// [`StopChildOutcome::Stopped`] means the stop is committed, not that
    /// the child has ended, and [`StopChildOutcome::Absorbed`] means a
    /// pending group restart covers the child.
    ///
    /// The child's slot stays taken until the child and every actor it
    /// spawned have ended: [`delete_child`](Self::delete_child) refuses
    /// while the stop is in progress, and a [`spawn_child`](Self::spawn_child)
    /// under the same id is refused with `DuplicateChild` while the
    /// supervisor holds the spec (the spec of a `Temporary` child, and of any
    /// child of a [`SimpleOneForOne`](crate::RestartStrategy::SimpleOneForOne)
    /// supervisor, goes when the supervisor classifies its end, the spec of
    /// any other child stays until [`delete_child`](Self::delete_child)); once
    /// the spec is gone, a spawn under the id waits for the subtree. A
    /// supervisor never waits for a child this way, which Erlang/OTP's
    /// `terminate_child` does.
    ///
    /// A child that outlasts its whole stop carries the Unresponsive verdict
    /// and no caller waits for it, so this call cannot return it: the
    /// supervisor stops with [`SupervisionError::ChildUnresponsive`] for the
    /// child, as it does when a group restart meets such a child, and its
    /// own supervisor decides.
    ///
    /// # Errors
    /// The set of [`stop_child`](Self::stop_child), except
    /// [`SupervisionError::ChildUnresponsive`], which the supervisor's own
    /// stop reports as above.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, StopChildOutcome,
    /// };
    ///
    /// struct Worker;
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = bool;
    ///     async fn handle(&mut self, _: (), ctx: &mut ActorContext<Self>) -> ActorResult<bool> {
    ///         let child = ctx.spawn_child(|| Worker).named("worker").await?;
    ///         // Returns before the child has ended.
    ///         let outcome = ctx.begin_stop_child(child.id().clone())?;
    ///         Ok(outcome == StopChildOutcome::Stopped)
    ///     }
    /// }
    ///
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().supervisor().await?;
    /// assert!(boss.send(()).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn begin_stop_child(
        &mut self,
        child: impl Into<ActorId>,
    ) -> Result<StopChildOutcome, SupervisionError> {
        self.manual_stop_commit(child.into(), ManualStop::Bounce)
            .map(|committed| committed.outcome)
    }

    /// Stops a supervised child without restarting it, as
    /// [`terminate_child`](Self::terminate_child) commits the stop, and
    /// returns at once: the commit without the wait for the child's end.
    ///
    /// The spec is kept or removed as `terminate_child` keeps or removes it,
    /// once the supervisor's death arm classifies the end, which reaches
    /// [`on_child_stopped`](crate::actor::Actor::on_child_stopped) once as
    /// [`SupervisionAction::Removed`](crate::SupervisionAction::Removed).
    /// The child's slot stays taken until the child and every actor it
    /// spawned have ended, and a [`spawn_child`](Self::spawn_child) under the
    /// same id is refused or waits as for
    /// [`begin_stop_child`](Self::begin_stop_child).
    /// A `significant` child stopped this way never shuts its supervisor down,
    /// as the supervisor ordered its end.
    ///
    /// A child that outlasts its whole stop ends the supervisor with
    /// [`SupervisionError::ChildUnresponsive`], as for
    /// [`begin_stop_child`](Self::begin_stop_child).
    ///
    /// # Errors
    /// The set of [`terminate_child`](Self::terminate_child), except
    /// [`SupervisionError::ChildUnresponsive`], which the supervisor's own
    /// stop reports.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult,
    /// };
    ///
    /// struct Worker;
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let child = ctx.spawn_child(|| Worker).named("worker").await?;
    ///         ctx.begin_terminate_child(child.id().clone())?;
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().supervisor().await?;
    /// boss.send(()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn begin_terminate_child(
        &mut self,
        child: impl Into<ActorId>,
    ) -> Result<(), SupervisionError> {
        self.manual_stop_commit(child.into(), ManualStop::Terminate)
            .map(|_| ())
    }

    /// Waits for the subtree of the last incarnation of `id` to end: the
    /// registered child's, or the removed child's that a start under the id
    /// would wait for. The wait is bounded by the budget that subtree carries
    /// and gives up with [`SupervisionError::ChildUnresponsive`] when a
    /// descendant outlives it.
    async fn await_child_subtree(&mut self, id: &ActorId) -> Result<(), SupervisionError> {
        let Some(subtree) = self
            .supervision
            .as_mut()
            .and_then(|sup| sup.predecessor_of(id))
        else {
            return Ok(());
        };
        if rules::successor_wait_step(subtree.is_gone(), false, false) == rules::WaitAction::Proceed
        {
            return Ok(());
        }
        match await_subtree(&subtree).await {
            rules::WaitAction::Escalate => Err(SupervisionError::ChildUnresponsive(id.clone())),
            _ => Ok(()),
        }
    }

    /// The commit and the wait on the child's own end of
    /// [`manual_stop_child`](Self::manual_stop_child).
    async fn manual_stop_decided(
        &mut self,
        id: ActorId,
        kind: ManualStop,
    ) -> Result<(StopChildOutcome, ManualStop), SupervisionError> {
        let committed = self.manual_stop_commit(id.clone(), kind)?;
        let effective = committed.effective;
        let Some(JoinTicket {
            mut cell,
            incarnation,
        }) = committed.join
        else {
            return Ok((committed.outcome, effective));
        };
        match await_cell(&mut cell, incarnation).await {
            CellOutcome::Fate(fate) => {
                drop(self.finish_manual_stop(&id, incarnation, effective, fate.reason));
                Ok((StopChildOutcome::Stopped, effective))
            }
            CellOutcome::Unresponsive => {
                cell.mark_reported();
                Err(SupervisionError::ChildUnresponsive(id))
            }
            // No manual stop reaches an attempt (a child with a restart in
            // flight answers `ChildRestarting`), so there is nothing to
            // classify.
            CellOutcome::Cancelled | CellOutcome::NoIncarnation => {
                Ok((StopChildOutcome::Stopped, effective))
            }
        }
    }

    /// The synchronous half of a manual stop: every effect of the call's
    /// commit up to the wait on the child's end, with no await.
    ///
    /// The ledger moves to `Stopping{kind}`, the first signal is raised and
    /// the stop order is written to the child's runner, so a caller that
    /// stops here has still made the stop happen. `join` is the ticket for
    /// the wait that follows, present exactly where the awaited calls wait
    /// for the child's fate cell. A caller that holds no ticket leaves the
    /// classification of the end to the supervisor's death arm, which
    /// finishes it for any stop still `Stopping` when the death arrives.
    pub(crate) fn manual_stop_commit(
        &mut self,
        id: ActorId,
        kind: ManualStop,
    ) -> Result<Committed, SupervisionError> {
        let sup = self
            .supervision
            .as_ref()
            .ok_or(SupervisionError::NotASupervisor)?;
        if sup.final_delivery {
            return Err(SupervisionError::SupervisorStopping);
        }
        let facts = sup.manual_facts(&id);
        let policy = facts.policy;
        // The incarnation the call concerns: the one a Running or Stopping
        // child holds (the effects that need it are only decided for those).
        let incarnation = facts
            .life
            .and_then(|life| life.occupant())
            .unwrap_or_default();
        let commit = manual_commit(kind, &facts);
        let kind = rules::effective_kind(kind, &facts);
        let done = |outcome, join| {
            Ok(Committed {
                outcome,
                effective: kind,
                join,
            })
        };

        if let Some(to) = commit.to {
            if let Some(child) = self
                .supervision_mut()
                .and_then(|sup| sup.registry.get_mut(&id))
            {
                child.transition_logged(to);
            }
        }
        let mut raised = false;
        // The factories the call removes, dropped after its last write (see
        // `remove_spec`): a panic in a drop is a panic of the calling callback.
        let mut removed: Vec<RestartFn> = Vec::new();
        for effect in commit.effects {
            match effect {
                CommitEffect::RaiseFirst => {
                    raised = self.with_child(&id, |child| child.ctl.raise_first(policy));
                }
                CommitEffect::WriteOrder => {
                    self.with_child(&id, |child| child.ctl.write_order(policy, raised));
                }
                CommitEffect::RecordOverride => {
                    if let Some(sup) = self.supervision_mut() {
                        sup.record_override(&id);
                    }
                }
                CommitEffect::LeaveRestartOrder => {
                    // The released member is taken out of the restart set at
                    // once, so nothing the group does later (the chain, a
                    // retry) can recapture a member the caller released.
                    if let Some(sup) = self.supervision_mut() {
                        sup.leave_restart_set(&id);
                    }
                }
                CommitEffect::Reask => self.with_child(&id, |child| child.ctl.ask_again()),
                CommitEffect::ReaskOnce => self.with_child(&id, |child| {
                    child.ctl.ask_again();
                    child.ctl.mark_reasked();
                }),
                CommitEffect::Dismiss => {
                    if let Some(sup) = self.supervision_mut() {
                        removed.extend(sup.dismiss_owed_evaluation(&id));
                    }
                }
                CommitEffect::RewriteSnapshot => {
                    if let Some(sup) = self.supervision_mut() {
                        sup.rewrite_restart_snapshot(&id);
                    }
                }
                CommitEffect::Classify => {
                    let reason = self.with_child(&id, |child| child.ctl.end_reason());
                    removed.extend(self.finish_manual_stop(&id, incarnation, kind, reason));
                }
                CommitEffect::Join => {
                    let cell = self.with_child(&id, |child| child.ctl.watch());
                    drop(removed);
                    return done(
                        StopChildOutcome::Stopped,
                        Some(JoinTicket { cell, incarnation }),
                    );
                }
                CommitEffect::Answer(reply) => {
                    drop(removed);
                    return manual_reply(reply, &id).map(|outcome| Committed {
                        outcome,
                        effective: kind,
                        join: None,
                    });
                }
            }
        }
        drop(removed);
        done(StopChildOutcome::Stopped, None)
    }

    /// Reads one fact of a registered child. Only called for effects whose
    /// decision named a child, so the child exists.
    fn with_child<R>(&self, id: &ActorId, read: impl FnOnce(&ChildState) -> R) -> R {
        let child = self
            .supervision
            .as_ref()
            .and_then(|sup| sup.registry.get(id))
            .expect("a manual stop effect names a registered child");
        read(child)
    }

    /// Finishes the classification of a manual stop whose child ended, as the
    /// caller of an awaited stop does once the child's fate cell resolves.
    /// A stop the supervisor's death arm classified already is left as it is.
    pub(crate) fn settle_manual_stop(
        &mut self,
        id: &ActorId,
        incarnation: u64,
        kind: ManualStop,
        reason: StopReason,
    ) {
        // The factory of a spec the classification removed is dropped as the
        // call's last step: a panic in its drop is a panic of the caller.
        drop(self.finish_manual_stop(id, incarnation, kind, reason));
    }

    /// Classifies a manual stop whose child ended, when the ledger still
    /// holds the occupant that was stopped: a stop in progress keeps the kind
    /// it was committed with (the in-flight kind decides the disposition),
    /// and an occupant a terminate override concerns takes the call's kind.
    /// An occupant the ledger has already moved past was classified by the
    /// run loop.
    #[must_use = "the factory is dropped by the caller, after its last write"]
    fn finish_manual_stop(
        &mut self,
        id: &ActorId,
        incarnation: u64,
        kind: ManualStop,
        reason: StopReason,
    ) -> Option<RestartFn> {
        let classify_as = self
            .supervision_ref()
            .and_then(|sup| sup.registry.get(id))
            .and_then(|child| match child.lifecycle {
                ChildLifecycle::Stopping {
                    incarnation: n,
                    kind: in_flight,
                } if n == incarnation => Some(in_flight),
                ChildLifecycle::Running(n) if n == incarnation => Some(kind),
                _ => None,
            });
        classify_as.and_then(|classify_as| {
            self.classify_manual_completion(id, incarnation, classify_as, reason)
        })
    }

    /// Classifies a manual stop's REAL, observed disposition once its fate
    /// cell resolves, as [`rules::classify_manual`] decides it, and either
    /// builds the `on_child_stopped` snapshot the run loop's death arm will
    /// deliver, or - for a `Bounce` racing a genuine `Failure` - records the
    /// end as owing its restart decision, so the death is instead evaluated
    /// as an ordinary crash (budget charge, strategy, restarts), exactly like
    /// any other failure. The ended incarnation is recorded down before a
    /// restart of the child begins.
    ///
    /// Called from two places: `manual_stop_child` itself, once its own
    /// await on the fate cell resolves, and - for cancel-safety - the run
    /// loop's death arm (`handle_child_stopped`), as a fallback for a caller
    /// that dropped its `terminate_child`/`stop_child` future right after the
    /// synchronous commit and before ever observing the fate: the ledger is
    /// still `Stopping{kind}` when the death arm gets there, so it finishes
    /// the classification itself instead of ever treating an un-classified
    /// manual stop as a plain, strategy-evaluated crash.
    ///
    /// Returns the factory of a spec it removed, once every write here is
    /// done, for the caller to drop after its own last write.
    #[must_use = "the factory is dropped by the caller, after its last write"]
    pub(crate) fn classify_manual_completion(
        &mut self,
        id: &ActorId,
        incarnation: u64,
        kind: ManualStop,
        reason: StopReason,
    ) -> Option<RestartFn> {
        let sup = self.supervision_mut()?;
        let pool = matches!(sup.config.strategy, RestartStrategy::SimpleOneForOne);
        let (restart_type, child_name) = sup.registry.get(id).map(|child| {
            (
                child.spec.restart_type,
                child.name.as_deref().map(str::to_string),
            )
        })?;
        // A member the pending group restarts, and not released by a
        // `terminate_child`: its end belongs to that restart. A member the
        // group only stops (a `Temporary` one) is not in the set.
        let in_restart_set = sup.in_restart_set(id);
        let outcome = rules::classify_manual(kind, &reason, restart_type, pool, in_restart_set);

        // The end of the ledger's occupant is recorded first: a bounce that
        // met a crash owes its restart decision, any other completion is
        // kept for the run loop's snapshot, and a spec that is not kept is
        // removed.
        let mut factory = None;
        if outcome.remove {
            factory = sup.remove_spec(id);
        } else if let Some(child) = sup.registry.get_mut(id) {
            child.transition_logged(ChildLifecycle::Down {
                incarnation,
                pending: outcome.pending,
            });
        }
        // A bounce that met a crash is evaluated as the crash it was: no
        // snapshot is made.
        let Some(action) = outcome.action else {
            return factory;
        };
        sup.pending_manual_events.insert(
            (id.clone(), incarnation),
            ChildEvent {
                child_id: id.clone(),
                child_name,
                reason,
                action,
            },
        );
        if outcome.initiate {
            // Budget-FREE restart: "manual stop is not a failure". The child
            // is down before the attempt begins.
            let seq = self.resolved_system().next_incarnation();
            self.initiate_attempt(id, seq, Origin::Bounce);
        }
        factory
    }
}
