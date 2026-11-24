//! Registered actors by name or id: typed lookup, enumeration, status, subtree observers, stop and kill.

use std::time::Duration;

use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::{
    raise_kill_declared, raise_noted, read_status, terminal_report, unresponsive,
    wait_end_or_floor, wait_terminal, ActorSystem, KillFrom, KillKey,
};
use crate::actor::handle::{emit_stop_requested, ActorHandle, SubtreeObserver};
use crate::actor::incarnation::{IncarnationRef, RaiseResult, StatusRx};
use crate::actor::supervision::model::{
    SystemAction, SystemEnd, SystemLadder, SystemSeen, SystemStep,
};
use crate::actor::supervision::rules::system_ladder_step;
use crate::actor::Actor;
use crate::error::SendError;
#[cfg(doc)]
use crate::error::SpawnError;
use crate::types::{ActorId, ActorStatus, StopOutcome, StopReason, StopReport};

impl ActorSystem {
    // - Actor lookup -------------------------------------------------------

    /// Looks up a named actor, returning a typed handle.
    ///
    /// Returns `None` if the name is not registered (as Erlang/OTP's `whereis/1`
    /// returns `undefined`) or if the registered actor is of a different type.
    ///
    /// A handle is bound to one incarnation of the actor: after a supervised
    /// restart the name resolves to the next incarnation's handle, so a
    /// caller that talks to a restartable actor looks it up by name for each
    /// call rather than keeping a handle. A lookup finds an incarnation
    /// until its registry entry is removed at the end of its teardown,
    /// including the time after its message loop ended, when the handle's
    /// [`is_alive`](ActorHandle::is_alive) reads `false` and a send returns
    /// `Closed`; a caller that meets that window waits with
    /// [`wait_stopped`](ActorHandle::wait_stopped) on the handle it holds and
    /// looks the name up again. The name is free from the removal of the
    /// entry on, and a spawn under it succeeds from then; a spawn right after
    /// a lookup found nothing can still answer [`SpawnError::NameTaken`] when
    /// the name is held by an actor of another type or another spawn took it
    /// first. A lookup right after a crashing request was
    /// answered (`AskError::Actor`) never finds that incarnation: the answer
    /// is sent once the name is free. A lookup that finds nothing while a
    /// supervisor restarts the actor reads as Erlang/OTP's `undefined` in the
    /// same window; retry after a short delay. For an actor that nothing
    /// restarts, `None` is final.
    pub fn get<A: Actor>(&self, name: &str) -> Option<ActorHandle<A>> {
        self.by_name(name).and_then(|e| e.downcast::<A>())
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
    ///
    /// # Names the application owns
    ///
    /// An application that wants to reach an actor by a name of its own keeps
    /// the map itself, from its names to ids, and resolves through this call.
    /// The map is typed and lives where the application wants it, and an id
    /// whose actor has ended finds nothing.
    ///
    /// ```rust
    /// use std::collections::HashMap;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorId, ActorResult, ActorSystem,
    /// };
    ///
    /// #[derive(Default)]
    /// struct Counter(i64);
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
    ///     let mut names: HashMap<&'static str, ActorId> = HashMap::new();
    ///     let counter = Counter::default().spawn().await?;
    ///     names.insert("tally", counter.id().clone());
    ///
    ///     let sys = ActorSystem::default();
    ///     let found = sys
    ///         .get_by_id::<Counter>(&names["tally"])
    ///         .expect("the actor is registered");
    ///     assert_eq!(found.send(2).await?, 2);
    ///     assert!(sys.get_by_id::<Counter>(&ActorId::from("other")).is_none());
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
            let entry = self.by_name(name).ok_or(SendError::NotFound)?;
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
    /// runs `on_stopped` and while its children are being stopped. The actor
    /// leaves the registry once its children are stopped (an actor
    /// ending by `Kill` does not wait for them: as its links drop, a worker
    /// child that does not trap exit signals is raised `Kill` and a supervisor
    /// child or a worker that traps them `ParentKilled`, which can come after
    /// the actor has left), before its stop lane is sealed and before its
    /// queued messages and its value are dropped (an aborted actor's when its
    /// task ends). This answers `NotFound` from the removal of the actor's
    /// registry entry on, and the name is free from the same instant. A `Kill` accepted after the actor left the registry
    /// arms no abort: its one remaining await is the end of its already
    /// aborted timer and stream tasks, which waits on their drops, so its end
    /// never precedes theirs.
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
            let entry = self.by_name(name).ok_or(SendError::NotFound)?;
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
            KillFrom::Plain,
        );
        if raised.result == RaiseResult::Refused {
            return Err(SendError::Closed);
        }
        Ok(())
    }

    /// Lists all registered actor names in this system.
    pub fn registered(&self) -> Vec<String> {
        self.by_id
            .iter()
            .filter(|e| e.named)
            .map(|e| e.key().0.as_str().to_string())
            .collect()
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
    ///
    /// # Counting actors
    ///
    /// The number of actors is the length of this list. The runtime the
    /// application owns reports its own load through
    /// [`tokio::runtime::RuntimeMetrics`], such as the number of workers and
    /// of live tasks, so a statistics view is the two read together.
    ///
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, ActorSystem,
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
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let _first = Worker.spawn().await?;
    ///     let _second = Worker.spawn().await?;
    ///
    ///     let sys = ActorSystem::default();
    ///     assert_eq!(sys.actor_ids().len(), 2);
    ///     let metrics = tokio::runtime::Handle::current().metrics();
    ///     assert!(metrics.num_workers() >= 1);
    ///     assert!(metrics.num_alive_tasks() >= 2);
    ///     Ok(())
    /// }
    /// ```
    pub fn actor_ids(&self) -> Vec<ActorId> {
        self.by_id.iter().map(|e| e.key().0.clone()).collect()
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
            KillFrom::Plain,
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

    /// Clones the lane of the incarnation whose status plane is `status_rx`
    /// out of the map guard and drops the guard before raising it. A missing
    /// entry (the actor already died on its own), or one that is a later
    /// incarnation under the same id, is a silent no-op: the caller's
    /// subsequent status wait finds the incarnation already terminal.
    pub(super) fn send_stop(&self, id: &ActorId, status_rx: &StatusRx, reason: StopReason) {
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
    pub(super) fn kill_registered(&self, id: &ActorId, status_rx: &StatusRx) -> bool {
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
            KillFrom::Plain,
        )
        .result
            != RaiseResult::Refused
    }

    /// Aborts the task of the incarnation whose status plane is `status_rx`.
    pub(super) fn abort_by_id(&self, id: &ActorId, status_rx: &StatusRx) {
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

/// Force-stop wait shared by `kill_by_id` and the concurrent sweep: the
/// target's kill grace -> `abort` (when there is a task to abort) -> settle
/// wait. `requested` is what a `Stopped` the actor publishes itself reads as
/// (`Killed` once a `Kill` was raised on it).
pub(super) async fn force_stop(
    mut status_rx: StatusRx,
    abort: Option<AbortHandle>,
    record: IncarnationRef,
    kill_grace: Duration,
    requested: StopOutcome,
) -> StopReport {
    let ladder = SystemLadder::Force { grace: kill_grace };
    let mut now = Instant::now().into_std();
    let begin = SystemSeen::Begin { accepted: true };
    let (mut step, mut action) = system_ladder_step(ladder, SystemStep::Start, begin, now);
    let ended_or_passed = |ended| {
        if ended {
            SystemSeen::Ended
        } else {
            SystemSeen::Passed {
                kill_started: false,
            }
        }
    };
    loop {
        let seen = match action {
            SystemAction::WaitUntil(until) => {
                let bound = until.saturating_duration_since(now);
                ended_or_passed(wait_terminal(&mut status_rx, bound).await)
            }
            SystemAction::Abort => {
                if let Some(abort) = &abort {
                    record.abort(abort);
                }
                SystemSeen::Taken { raised: false }
            }
            // Without an abort of its own the force follows the one the
            // reaper issues: no verdict before that abort plus the settle
            // time. A wait for an owed abort is capped by the force's whole
            // budget once more.
            SystemAction::WaitFloor { budget, cap } => {
                ended_or_passed(wait_end_or_floor(&mut status_rx, Some(&record), budget, cap).await)
            }
            SystemAction::Finish(end) => {
                return match end {
                    SystemEnd::Terminal { .. } => terminal_report(&status_rx, requested),
                    SystemEnd::Unresponsive => unresponsive(),
                    SystemEnd::Refused | SystemEnd::Sweep { .. } => {
                        unreachable!("a force stop is neither refused nor swept")
                    }
                };
            }
            SystemAction::Request | SystemAction::Kill { .. } | SystemAction::None => {
                unreachable!("a force stop takes no such step")
            }
        };
        now = Instant::now().into_std();
        (step, action) = system_ladder_step(ladder, step, seen, now);
    }
}
