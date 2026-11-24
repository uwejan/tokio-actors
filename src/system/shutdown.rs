//! System shutdown: the walk over the roots, the waits on their ends and the report.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant;

#[cfg(test)]
use super::LadderPoint;
use super::{
    force_stop, raise_kill_declared, read_status, ActorSystem, KillFrom, KillKey, ReaperEntry,
    ReaperSlot, ShutdownPolicy, SystemPhase,
};
use crate::actor::incarnation::{IncarnationRef, RaiseResult, StatusRx};
use crate::actor::panic::{contain_waker_panic, send_contained};
use crate::actor::runtime::{bounded_wait, saturating_deadline};
use crate::actor::supervision::model::{
    SystemAction, SystemEnd, SystemLadder, SystemSeen, SystemStep,
};
use crate::actor::supervision::rules::system_ladder_step;
#[cfg(doc)]
use crate::error::SpawnError;
use crate::helper::HelperKind;
use crate::types::{ActorId, ActorStatus, ShutdownReport, StopOutcome, StopReason, StopReport};

impl ActorSystem {
    // - Shutdown -------------------------------------------------------------

    /// Shuts down all registered actors using the system's stored policy.
    /// See [`shutdown_with`](Self::shutdown_with); with a root to wait on it
    /// needs a Tokio runtime with the time driver enabled.
    pub async fn shutdown(&self) -> ShutdownReport {
        self.shutdown_with(self.shutdown_policy.clone()).await
    }

    /// Shuts down the system: stops every ROOT actor (one spawned via the
    /// top-level `SpawnBuilder`, not a supervised child) in reverse
    /// registration order, sequentially, escalating from
    /// `StopReason::ParentRequest` up through `Kill` and finally a task abort
    /// as needed. Each root's own supervision cascade takes its subtree down
    /// in turn: the walk takes the roots down their ladders, and every other
    /// actor is stopped by its supervisor (when a supervisor ends by a `Kill`
    /// or an abort, the link guards of its children stop them) or by the
    /// walk's final sweep described below. A supervisor evaluates restarts
    /// only in its own message loop, so once that loop has exited no restart
    /// of its children is begun.
    ///
    /// New registrations are rejected with `SpawnError::SystemShuttingDown`
    /// from the first poll of this call.
    /// The system reads `Defunct` once a shutdown has completed since it was
    /// last `Active` and no shutdown is in flight any more. Shutdowns cut
    /// short before one completes (see "The walk and its caller" below)
    /// leave the system `ShuttingDown`, refusing registrations and with
    /// [`reactivate`](Self::reactivate) returning `false`, until a shutdown
    /// completes; after that the system keeps refusing registrations, and
    /// `reactivate` works again whenever no shutdown is in flight, however
    /// later calls ended.
    ///
    /// Bounded by `policy.timeout`: once the global deadline passes, every
    /// remaining root is force-stopped (`Kill` + `abort`) CONCURRENTLY instead
    /// of sequentially, so this always returns close to the deadline regardless
    /// of how many roots are still outstanding. A final sweep handles anything
    /// still left in the registry afterward (normally nothing: supervised
    /// children vanish via their own guard drops during the cascade above). The
    /// sweep waits on every leftover at once, each wait bounded by the global
    /// deadline, so a supervisor still stopping its children in order after its
    /// own supervisor ended can finish; every leftover still running at the
    /// global deadline is then force-stopped (`Kill` + `abort`) concurrently. A
    /// leftover that nothing stops is therefore held until the global deadline
    /// and then killed. After the global deadline, a shutdown can run on for
    /// the longest kill grace among the roots it forces plus the abort settle
    /// time, then the longest kill grace among the leftovers it forces plus the
    /// abort settle time, and, for either of those two steps, that time once
    /// more plus the abort settle time when the abort of an actor it forces,
    /// scheduled with the system, is issued late. That deadline `Kill`, and the
    /// abort one kill grace after it (counted from when the `Kill` fires), are
    /// scheduled with the system when the sweep begins, on the runtime of each
    /// leftover whose task is already spawned, so they happen even if the walk
    /// is cut short before the deadline. Since a stop only ever strengthens,
    /// the system holds one sweep deadline: a sweep lowers it to its own when
    /// that is earlier, but never to before the moment it lowers it, and every
    /// sweep, one already waiting in another call included, waits on and
    /// reports its leftovers by it. It is cleared only when the last shutdown
    /// in flight ends after a shutdown has completed since the system was last
    /// `Active` (shutdowns that are all cut short before one completes leave
    /// it, as they leave `ShuttingDown`). A shutdown made after only cut-short
    /// ones folds into the stored deadline, which may already have passed, and
    /// then forces its leftovers at once.
    ///
    /// A leftover whose task ended without publishing `Stopped` reads
    /// `Aborted`. One that published `Stopped` reads by the instant its end
    /// became final (its stop lane's seal) against the system's deadline:
    /// `Killed` when that instant is at or after the deadline (or there is
    /// none) or when this shutdown's force took a `Kill` on the leftover, and
    /// `Graceful`, with the reason it ended with, when it is before the
    /// deadline. A leftover reads `Unresponsive` when even the abort did not
    /// end it within the settle time after it. The instant does not
    /// depend on which call looks or when, so concurrent calls report one end
    /// the same way.
    ///
    /// Needs a Tokio runtime with the time driver enabled (`enable_time` or
    /// `enable_all`) that is driven, not only entered: the walk is a task of
    /// the system on that runtime. Without the driver the call panics at its
    /// first wait on a root, after the system has entered shutdown, and a
    /// leftover whose wait fails is forced at once and reported from its
    /// plane; a system with no root completes.
    ///
    /// Registry entries are removed as their actors die (the existing
    /// `RegistryGuard` drop machinery) - never cleared up front, so a lookup
    /// racing the early part of shutdown still finds a live actor.
    ///
    /// # The walk and its caller
    /// The call enters the shutdown, so that registrations are refused, in
    /// its first poll, and then runs the walk in a task of the system on the
    /// runtime that is current at the call; the caller only waits for the
    /// walk's report. The walk therefore survives its caller's future: a call
    /// that is dropped, abandoned, stored and never polled again, or made from
    /// inside an actor that the walk stops, does not stop the walk, which
    /// takes each root down its ladder (a stop request, then a `Kill` once its
    /// time has passed, then an abort; once the global deadline has passed,
    /// the remaining roots are force-stopped with a `Kill` and an abort),
    /// completes, and leaves the system `Defunct` once no other shutdown is in
    /// flight; a root that does not yield even to the abort is reported
    /// `Unresponsive` and may still be running. A call from inside a root is
    /// stopped with that root like any other pending handler, and the walk goes
    /// on to the roots after it. Several calls at once each run their own walk,
    /// and their sweep deadlines fold into one as described above.
    ///
    /// OTP parity: `init:stop/0` is a request to a system process, `init`,
    /// which owns the node's shutdown and returns to the caller at once; here
    /// the walk is likewise a task of the system, and the caller waits for its
    /// report. The first two steps of each root's escalation follow
    /// `shutdown/1` in `supervisor.erl`, which runs in the supervisor process:
    /// a stop request, then a `Kill` (the counterpart of `exit(Pid, kill)`)
    /// once its time has passed. The abort that follows, as the backstop for a
    /// callback that does not reach a turn boundary, has no OTP counterpart,
    /// since `exit(Pid, kill)` needs no cooperation from the process. OTP waits
    /// without limit for the `'DOWN'` after its kill; here the walk settles
    /// after a short time and reports the root `Unresponsive`: a declared
    /// departure.
    ///
    /// The walk does not survive its own runtime. When the runtime it runs on
    /// ends before the walk does and nothing is still waiting on the call, the
    /// walk stops there: it asks no further
    /// root to stop, so the roots on other runtimes that it had not yet
    /// reached keep running (roots on the ended runtime end with it), and the
    /// system keeps refusing
    /// every supervised restart as well as every new registration with
    /// [`SpawnError::SystemShuttingDown`], and a refused restart is charged to
    /// the restart budget like any failed restart: a supervisor whose child
    /// then dies and is due a restart exhausts a budget whose window holds its
    /// `max_restarts` restarts, while a zero window with a `max_restarts`
    /// above zero keeps retrying until a shutdown reaches the supervisor,
    /// unless the restarts read the same instant (see
    /// [`SupervisionConfig::restart_window`](crate::actor::supervision::SupervisionConfig::restart_window)).
    /// [`reactivate`](Self::reactivate) works again once a shutdown of the
    /// system has completed (an earlier one counts) and none is in flight, and
    /// a later call to [`shutdown`](Self::shutdown) can complete one. If the
    /// task ends without a report while its caller still waits (the walk
    /// panicked, or the runtime it ran on ended while the call was polled from
    /// outside it), the caller walks the system itself in its own future and
    /// returns that report if that walk completes; a walk that needs a timer on
    /// an ended runtime panics at its first wait on a root that is still
    /// running. A call made with no Tokio runtime current walks in the
    /// caller's future, and dropping that future stops the walk. With no
    /// runtime current the call completes only for a system with no root and no
    /// leftover: once a root exists, it panics at its first wait on it, because
    /// that wait uses a timer, which needs a driven runtime with the time
    /// driver. The system keeps refusing registrations.
    pub async fn shutdown_with(&self, policy: ShutdownPolicy) -> ShutdownReport {
        #[cfg(test)]
        self.shutdown_at_the_gate.store(true, Ordering::SeqCst);
        let deadline = saturating_deadline(Instant::now(), policy.timeout);
        let Some((rt, owner)) = self.walk_host() else {
            // No runtime is current, or the system is not the one the global
            // map holds: the caller drives the walk.
            let in_flight = ShutdownInFlight::enter(self);
            let roots = self.first_snapshot();
            return self.walk(policy, deadline, roots, in_flight).await;
        };
        // The phase change and the first snapshot happen here, in the
        // caller's first poll: a registration attempted right after it is
        // refused, and every root registered before it is in the snapshot.
        let in_flight = ShutdownInFlight::enter(owner.clone());
        let roots = owner.first_snapshot();
        let (report_tx, report_rx) = oneshot::channel();
        let report_tx = WalkReport(Some(report_tx));
        let walk_policy = policy.clone();
        let walker = owner.clone();
        drop(
            owner.spawn_helper(&rt, HelperKind::ShutdownWalk, None, move |end| {
                end.observe(async move {
                    let report = walker.walk(walk_policy, deadline, roots, in_flight).await;
                    // The helper's hold on the system ends before the caller
                    // is woken, so a caller that drops its own handle right
                    // after the call is the last holder.
                    drop(walker);
                    report_tx.send(report);
                })
            }),
        );
        match report_rx.await {
            Ok(report) => report,
            Err(_) => {
                // The walk helper ended without a report (it panicked, or its
                // runtime ended): the caller walks the system itself.
                let in_flight = ShutdownInFlight::enter(self);
                let roots = self.first_snapshot();
                self.walk(policy, deadline, roots, in_flight).await
            }
        }
    }

    /// The runtime and the owning handle a shutdown walk runs on, or `None`
    /// when no runtime is current or this system is not the one the global
    /// map holds under its name.
    pub(super) fn walk_host(&self) -> Option<(Handle, Arc<ActorSystem>)> {
        let rt = Handle::try_current().ok()?;
        let held = Self::get_named(&self.name)?;
        (held.generation == self.generation).then_some((rt, held))
    }

    /// The roots a shutdown that has just entered takes on first, in the
    /// order it stops them: the most recently registered first.
    fn first_snapshot(&self) -> Vec<RootSnapshot> {
        let mut roots = self.snapshot_roots();
        roots.sort_by_key(|r| std::cmp::Reverse(r.registration_seq));
        roots
    }

    /// The shutdown walk: stops `roots`, sweeps the leftovers and, when it
    /// completes, releases `in_flight` as a completed shutdown.
    async fn walk<S: std::ops::Deref<Target = ActorSystem>>(
        &self,
        policy: ShutdownPolicy,
        deadline: Instant,
        roots: Vec<RootSnapshot>,
        mut in_flight: ShutdownInFlight<S>,
    ) -> ShutdownReport {
        // Every root incarnation this shutdown has taken on, by its
        // registration sequence (unique to one registration, so to one
        // incarnation): a later incarnation registered under a reported id
        // is a root of its own, not one already reported.
        let mut reported: HashSet<u64> = roots.iter().map(|r| r.registration_seq).collect();
        let mut outcomes = self
            .stop_roots(&roots, policy.per_actor_timeout, deadline)
            .await;

        // Every registration landed before the phase change above or is
        // refused while any shutdown is in flight (see `registration_gate`),
        // so the first snapshot holds every root a public call can produce
        // and this pass finds none through them. It stays as a defensive
        // branch: a registration that bypasses the gate (a test helper) is
        // still stopped. Each pass re-snapshots the roots and stops any root
        // incarnation not yet taken on; it ends once a snapshot holds nothing
        // new.
        loop {
            let mut late: Vec<RootSnapshot> = self
                .snapshot_roots()
                .into_iter()
                .filter(|r| !reported.contains(&r.registration_seq))
                .collect();
            if late.is_empty() {
                break;
            }
            late.sort_by_key(|r| std::cmp::Reverse(r.registration_seq));
            reported.extend(late.iter().map(|r| r.registration_seq));
            outcomes.extend(
                self.stop_roots(&late, policy.per_actor_timeout, deadline)
                    .await,
            );
        }
        #[cfg(test)]
        self.root_snapshots_done.store(true, Ordering::SeqCst);

        // Final sweep: anything still registered that wasn't already taken
        // on above. Normally empty - supervised children vanish via their
        // own guard drops as part of each root's cascade. A leftover can be
        // a supervisor whose own supervisor ended before it did and that is
        // stopping its children in order, or is about to: a killed parent's
        // child is raised its stop only when the runtime drops the runner
        // task its parent aborted, and a grandchild only when its own
        // parent's teardown reaches it, so nothing read here tells a
        // leftover about to stop from one that nothing stops. Every leftover
        // is therefore waited on until the global deadline, since a `Kill`
        // would cut an orderly stop short; those still running then are
        // force-stopped concurrently. These are non-root leftovers, so they
        // are reported in `swept`, never `outcomes`.
        //
        // The deadline's `Kill` and abort do not depend on this future: for
        // each leftover the system schedules, at the snapshot, a two-stage
        // entry on the reaper of the leftover's runtime, built from the
        // registration found (its own lane, task and kill grace, so a later
        // registration under the same id is never touched): `Kill` at the
        // global deadline, the abort one kill grace after it. An entry whose
        // task has ended by a stage does nothing at that stage. The force
        // sweep below still runs at the deadline and reports the outcomes.
        //
        // The deadline belongs to the system: this sweep folds its own into
        // the system's (see `fold_sweep_deadline`), arms and waits by the
        // result, and its waits follow the system's deadline while they run,
        // so a sweep that starts later with an earlier deadline shortens them
        // too. Every ended leftover is classified against the system's
        // deadline by the instant its end became final, which no call's
        // timing changes: one end gets one outcome whichever call reports
        // it.
        let deadline = self.fold_sweep_deadline(deadline);
        let mut deadline_kills: Vec<(Handle, ReaperEntry)> = Vec::new();
        let leftover: Vec<SweepTarget> = self
            .by_id
            .iter()
            .filter(|e| !reported.contains(&e.registration_seq))
            .map(|e| {
                if let Some(abort) = e.abort.clone() {
                    deadline_kills.push((
                        e.runtime.clone(),
                        ReaperEntry::two_stage(
                            deadline,
                            e.stop_lane.clone(),
                            saturating_deadline(deadline, e.kill_grace),
                            abort,
                        ),
                    ));
                }
                SweepTarget {
                    id: e.key().0.clone(),
                    snapshot: RootSnapshot {
                        id: e.key().0.clone(),
                        registration_seq: e.registration_seq,
                        status_rx: e.status_rx.clone(),
                        kill_grace: e.kill_grace,
                        record: e.stop_lane.abort_record().clone(),
                    },
                    kill_raised: false,
                }
            })
            .collect();
        for (runtime, entry) in deadline_kills {
            self.feed_reaper(&runtime, entry);
        }
        // Every leftover is waited on at once, each wait bounded by the
        // system's deadline. A wait task that fails (it panics or is
        // cancelled) has its leftover read as not seen to end, so it goes to
        // the force: every leftover is reported once.
        let mut waits = JoinSet::new();
        let mut by_task: HashMap<tokio::task::Id, (usize, SweepTarget)> = HashMap::new();
        for (index, target) in leftover.into_iter().enumerate() {
            let mut rx = target.snapshot.status_rx.clone();
            let mut system_deadline = self.sweep_deadline.subscribe();
            let task = waits.spawn(async move {
                wait_end_by_the_system_deadline(&mut rx, &mut system_deadline, deadline).await
            });
            by_task.insert(task.id(), (index, target));
        }
        let mut waited = Vec::new();
        while let Some(joined) = waits.join_next_with_id().await {
            let (task, ended) = match joined {
                Ok((task, ended)) => (task, ended),
                Err(err) => (err.id(), false),
            };
            if let Some((index, target)) = by_task.remove(&task) {
                waited.push((index, target, ended));
            }
        }
        waited.sort_by_key(|(index, ..)| *index);
        let mut swept: Vec<(ActorId, StopReport)> = Vec::new();
        let mut stragglers: Vec<SweepTarget> = Vec::new();
        for (_, target, ended) in waited {
            // A leftover that ended is reported from the plane it was found
            // with, never looked up by id again (its registry entry may be
            // gone).
            if ended {
                let report = leftover_report(
                    &target.snapshot.status_rx,
                    false,
                    self.current_sweep_deadline(deadline),
                );
                swept.push((target.id, report));
            } else {
                stragglers.push(target);
            }
        }
        if !stragglers.is_empty() {
            swept.extend(
                self.concurrent_force_sweep(stragglers, Some(deadline))
                    .await,
            );
        }

        // Shutdown has fully completed. Every walk snapshots after its own
        // entry, and registrations are refused from the first entry after
        // the system was last `Active`, so this walk has stopped every root
        // registered before that. The last shutdown in flight to end, this
        // one or a cut-short one that ends later, therefore moves the system
        // to `Defunct` rather than leaving `ShuttingDown` set forever, so
        // `reactivate` can return this system's name to service, and clears
        // the system's sweep deadline in the same step.
        in_flight.completed = true;
        drop(in_flight);

        ShutdownReport { outcomes, swept }
    }

    /// Folds a sweep's own deadline into the system's and returns the result,
    /// the deadline this sweep arms and waits by. The system's deadline becomes
    /// the earlier of the stored one and this sweep's own, where this sweep's
    /// own is first raised to an instant just after now: a lowering by this
    /// call then cannot land on an end already final, so an end compared with
    /// the system's deadline before the lowering and after it reads one way. A
    /// stored deadline already past stays as it is.
    pub(super) fn fold_sweep_deadline(&self, own: Instant) -> Instant {
        let floor = own.max(saturating_deadline(Instant::now(), Duration::from_nanos(1)));
        let mut folded = floor;
        self.sweep_deadline.send_modify(|stored| {
            folded = stored.map_or(floor, |earlier| earlier.min(floor));
            *stored = Some(folded);
        });
        folded
    }

    /// The system's sweep deadline as it stands now, or `own` when none is
    /// held. A sweep in flight keeps a shutdown counted, so its own folded
    /// value is held until it completes.
    pub(super) fn current_sweep_deadline(&self, own: Instant) -> Instant {
        self.sweep_deadline
            .borrow()
            .map_or(own, |held| held.min(own))
    }

    /// Stops `roots` one at a time through the sequential ladder, in the
    /// order given. The moment the global deadline cuts a root's ladder, that
    /// root and every root after it go to one concurrent force sweep instead,
    /// the cut root marked with whether its ladder had raised `Kill`.
    async fn stop_roots(
        &self,
        roots: &[RootSnapshot],
        per_actor_timeout: Duration,
        deadline: Instant,
    ) -> Vec<(ActorId, StopReport)> {
        let mut outcomes = Vec::with_capacity(roots.len());
        for (idx, root) in roots.iter().enumerate() {
            match self
                .stop_root_sequential(root, per_actor_timeout, deadline)
                .await
            {
                Ok(outcome) => outcomes.push((root.id.clone(), outcome)),
                Err(raised_kill) => {
                    let targets = roots[idx..]
                        .iter()
                        .enumerate()
                        .map(|(n, root)| SweepTarget {
                            id: root.id.clone(),
                            snapshot: root.clone(),
                            kill_raised: n == 0 && raised_kill,
                        })
                        .collect();
                    outcomes.extend(self.concurrent_force_sweep(targets, None).await);
                    break;
                }
            }
        }
        outcomes
    }

    /// Stops one root, the incarnation whose plane is `status_rx`, exactly as
    /// the shutdown walk stops a root: through [`stop_roots`](Self::stop_roots)
    /// under this system's stored policy, so a `ParentRequest` its `pre_stop`
    /// may veto until the per-root timeout, then `Kill`, then an abort, and
    /// the force sweep once the policy's global deadline passes. The system's
    /// phase is not touched. `None` when the registry holds no root of that
    /// incarnation: it has ended or is ending, or it is a supervised child.
    pub(crate) async fn stop_one_root(
        &self,
        id: &ActorId,
        status_rx: &StatusRx,
    ) -> Option<StopReport> {
        let root = self.root_of(id, status_rx)?;
        let policy = self.shutdown_policy.clone();
        let deadline = saturating_deadline(Instant::now(), policy.timeout);
        self.stop_roots(&[root], policy.per_actor_timeout, deadline)
            .await
            .pop()
            .map(|(_, report)| report)
    }

    /// The abort handle, the kill grace and the runtime of the incarnation
    /// whose plane is `status_rx`, when the registry holds it under `id` as a
    /// root, an actor with no supervisor. The handle is `None` until the spawn
    /// attached it.
    pub(crate) fn root_task(
        &self,
        id: &ActorId,
        status_rx: &StatusRx,
    ) -> Option<(Option<AbortHandle>, Duration, Handle)> {
        self.by_id
            .get(id)
            .filter(|e| e.parent.is_none() && e.status_rx.same_channel(status_rx))
            .map(|e| (e.abort.clone(), e.kill_grace, e.runtime.clone()))
    }

    /// Raises `Kill` on the root incarnation whose plane is `status_rx`
    /// through the registry's declaring raise, so the reaper aborts the task
    /// after its kill grace with no caller left to wait. Returns whether the
    /// lane took it; a root that has left the registry takes nothing.
    pub(crate) fn kill_root(&self, id: &ActorId, status_rx: &StatusRx) -> bool {
        let Some(lane) = self
            .by_id
            .get(id)
            .filter(|e| e.parent.is_none() && e.status_rx.same_channel(status_rx))
            .map(|e| e.stop_lane.clone())
        else {
            return false;
        };
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

    fn root_of(&self, id: &ActorId, status_rx: &StatusRx) -> Option<RootSnapshot> {
        self.by_id
            .get(id)
            .filter(|e| e.parent.is_none() && e.status_rx.same_channel(status_rx))
            .map(|e| RootSnapshot {
                id: e.key().0.clone(),
                registration_seq: e.registration_seq,
                status_rx: e.status_rx.clone(),
                kill_grace: e.kill_grace,
                record: e.stop_lane.abort_record().clone(),
            })
    }

    /// Snapshots every currently-registered ROOT actor's id, registration
    /// sequence, a clone of its status receiver and its kill grace. The
    /// snapshot is independent of the registry entry from this point on: the
    /// `status_rx` clone stays valid even after the entry is removed by a
    /// `RegistryGuard` drop, and it names exactly this incarnation.
    fn snapshot_roots(&self) -> Vec<RootSnapshot> {
        self.by_id
            .iter()
            .filter(|e| e.parent.is_none())
            .map(|e| RootSnapshot {
                id: e.key().0.clone(),
                registration_seq: e.registration_seq,
                status_rx: e.status_rx.clone(),
                kill_grace: e.kill_grace,
                record: e.stop_lane.abort_record().clone(),
            })
            .collect()
    }

    /// Runs one root through the escalation ladder (`ParentRequest` -> `Kill`
    /// -> `abort()`), each wait bounded by whichever is tighter: the step's
    /// own bound, or the time left before the global deadline. Every step
    /// acts only on the incarnation `root` snapshotted: a later registration
    /// under the same id is never signalled or aborted. The verdict is
    /// `Killed` only once this ladder raised `Kill` on the incarnation; an
    /// incarnation that left the registry before the `Kill` step ends with
    /// the outcome it reaches by itself. Returns `Err(kill_raised)` the
    /// instant the global deadline is hit without having reached a terminal
    /// status, saying whether this ladder had raised `Kill` by then - the
    /// caller then hands this root (and every root after it) to the
    /// concurrent force sweep instead of continuing the sequential ladder.
    async fn stop_root_sequential(
        &self,
        root: &RootSnapshot,
        per_actor_timeout: Duration,
        deadline: Instant,
    ) -> Result<StopReport, bool> {
        let id = &root.id;
        let mut status_rx = root.status_rx.clone();
        let ladder = SystemLadder::Walk {
            deadline: deadline.into_std(),
            per_actor: per_actor_timeout,
            grace: root.kill_grace,
        };
        let mut now = Instant::now().into_std();
        let begin = SystemSeen::Begin { accepted: true };
        let (mut step, mut action) = system_ladder_step(ladder, SystemStep::Start, begin, now);
        loop {
            let seen = match action {
                // Step 1: ParentRequest (vetoable via pre_stop).
                SystemAction::Request => {
                    #[cfg(test)]
                    self.run_ladder_hook(LadderPoint::Stop, id);
                    self.send_stop(id, &status_rx, StopReason::ParentRequest);
                    SystemSeen::Taken { raised: false }
                }
                // Step 2: Kill (unvetoable, but still only observed
                // cooperatively - an actor stuck inside a callback never
                // reaches the select! that would see it). The system
                // schedules the abort when it raises the Kill, so a walk cut
                // short from here on still ends in one.
                SystemAction::Kill { raise } => {
                    #[cfg(test)]
                    self.run_ladder_hook(LadderPoint::Kill, id);
                    let kill_raised = raise && self.kill_registered(id, &status_rx);
                    SystemSeen::Taken {
                        raised: kill_raised,
                    }
                }
                // Step 3: abort() - the last resort for an actor that never
                // yielded back to the run loop (so Kill was never even
                // observed).
                SystemAction::Abort => {
                    #[cfg(test)]
                    self.run_ladder_hook(LadderPoint::Abort, id);
                    self.abort_by_id(id, &status_rx);
                    SystemSeen::Taken { raised: false }
                }
                SystemAction::WaitUntil(until) => {
                    let bound = until.saturating_duration_since(now);
                    if wait_terminal(&mut status_rx, bound).await {
                        SystemSeen::Ended
                    } else {
                        SystemSeen::Passed {
                            kill_started: false,
                        }
                    }
                }
                SystemAction::Finish(end) => {
                    return match end {
                        SystemEnd::Terminal { killed } => {
                            let requested = if killed {
                                StopOutcome::Killed
                            } else {
                                StopOutcome::Graceful
                            };
                            Ok(terminal_report(&status_rx, requested))
                        }
                        // Nothing left to hand off to: the concurrent sweep
                        // would do exactly what steps 2-3 just did. A
                        // genuinely non-yielding task.
                        SystemEnd::Unresponsive => Ok(unresponsive()),
                        SystemEnd::Sweep { kill_raised } => Err(kill_raised),
                        SystemEnd::Refused => unreachable!("a walk's request is never refused"),
                    };
                }
                SystemAction::WaitFloor { .. } | SystemAction::None => {
                    unreachable!("a walk takes no such step")
                }
            };
            now = Instant::now().into_std();
            (step, action) = system_ladder_step(ladder, step, seen, now);
        }
    }

    /// Force-stops a batch CONCURRENTLY: `Kill` -> grace -> `abort()` ->
    /// grace, skipping the vetoable `ParentRequest` step entirely. Used once
    /// the global deadline is breached (remaining roots) and for the
    /// leftovers the final sweep still finds running at the global deadline:
    /// in both cases the time for a graceful stop has run out. Every id is
    /// reported exactly once, and no target's wait holds up another's.
    ///
    /// A target is looked up as the incarnation its snapshot kept. One the
    /// registry does not hold as that incarnation is waited on through the
    /// status receiver its snapshot kept, bounded as a found target is (its
    /// kill grace, then the wait after an abort), because the runtime frees
    /// an actor's registry entry before it publishes `Stopped`. A root's
    /// terminal end goes through the one terminal classifier: `Stopped`
    /// holds as `Killed` when this shutdown had raised `Kill` on it and as
    /// `Graceful` otherwise, a sender closed without `Stopped` is `Aborted`.
    /// A leftover's (`sweep_deadline` is the sweep's deadline) goes through
    /// the leftover classifier, [`leftover_report`], against the system's
    /// deadline read when its force ends. One still open after the bound is
    /// `Unresponsive`. A force task that fails (it panics or is cancelled)
    /// still has its target reported, from the target's plane: as its
    /// classifier reads it once the plane is terminal, `Unresponsive` while
    /// it still runs.
    async fn concurrent_force_sweep(
        &self,
        targets: Vec<SweepTarget>,
        sweep_deadline: Option<Instant>,
    ) -> Vec<(ActorId, StopReport)> {
        let mut set = JoinSet::new();
        let count = targets.len();
        // Each task's target, its plane, the outcome its classifier asks for
        // and whether this shutdown raised `Kill` on it, fixed here before
        // the task is spawned.
        let mut by_task: HashMap<tokio::task::Id, (ActorId, StatusRx, StopOutcome, bool)> =
            HashMap::new();
        for target in targets {
            let SweepTarget {
                id,
                snapshot,
                kill_raised,
            } = target;
            #[cfg(test)]
            self.run_ladder_hook(LadderPoint::SweepLookup, &id);
            let found = self
                .by_id
                .get(&id)
                .filter(|e| e.status_rx.same_channel(&snapshot.status_rx))
                .map(|e| (e.status_rx.clone(), e.stop_lane.clone(), e.kill_grace));
            let Some((status_rx, lane, grace)) = found else {
                // The registry does not hold the incarnation the sweep is after.
                let requested = if kill_raised {
                    StopOutcome::Killed
                } else {
                    StopOutcome::Graceful
                };
                let rx = snapshot.status_rx.clone();
                let record = snapshot.record.clone();
                let grace = snapshot.kill_grace;
                let task =
                    set.spawn(async move { force_stop(rx, None, record, grace, requested).await });
                by_task.insert(task.id(), (id, snapshot.status_rx, requested, kill_raised));
                continue;
            };
            #[cfg(test)]
            self.run_after_kill_lookup();
            // A lane that refuses the `Kill` belongs to an incarnation
            // already past its last read of it: nothing is armed, it ends
            // with its own reason, and the verdict is `Killed` only if this
            // shutdown raised `Kill` on it earlier. The wait is the
            // incarnation's own grace, read with the lookup, so it holds
            // after the entry is gone.
            let raised_kill = raise_kill_declared(
                Some(self),
                KillKey::Id(&id),
                &status_rx,
                &lane,
                KillFrom::Plain,
            );
            let took = raised_kill.result != RaiseResult::Refused;
            let raised = took || kill_raised;
            let requested = if raised {
                StopOutcome::Killed
            } else {
                StopOutcome::Graceful
            };
            let record = lane.abort_record().clone();
            let abort = if took {
                raised_kill.abort
            } else {
                self.kill_target_for(&id, &status_rx)
                    .and_then(|(abort, _, _)| abort)
            };
            let rx = status_rx.clone();
            let task =
                set.spawn(async move { force_stop(rx, abort, record, grace, requested).await });
            by_task.insert(task.id(), (id, status_rx, requested, raised));
        }

        let mut results = Vec::with_capacity(count);
        while let Some(joined) = set.join_next_with_id().await {
            let (task, report) = match joined {
                Ok((task, report)) => (task, Some(report)),
                Err(err) => (err.id(), None),
            };
            let Some((id, rx, requested, raised)) = by_task.remove(&task) else {
                continue;
            };
            let terminal = match &report {
                Some(report) => report.outcome != StopOutcome::Unresponsive,
                None => read_status(&rx) == ActorStatus::Stopped,
            };
            let report = if !terminal {
                report.unwrap_or_else(unresponsive)
            } else if let Some(deadline) = sweep_deadline {
                leftover_report(&rx, raised, self.current_sweep_deadline(deadline))
            } else {
                report.unwrap_or_else(|| terminal_report(&rx, requested))
            };
            results.push((id, report));
        }
        results
    }
}

/// The channel a shutdown walk run by a helper task reports on. The report is
/// sent once, contained; a walk that ends without one (it panicked, or its
/// runtime ended) drops the sender, which wakes the shutdown's caller, whose
/// waker belongs to the caller's task. That drop can run while the walk
/// unwinds, so a panic of the waker is contained there too.
struct WalkReport(Option<oneshot::Sender<ShutdownReport>>);

impl WalkReport {
    fn send(mut self, report: ShutdownReport) {
        if let Some(tx) = self.0.take() {
            let _ = send_contained(tx, report);
        }
    }
}

impl Drop for WalkReport {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            contain_waker_panic("shutdown report", None, move || drop(tx));
        }
    }
}

/// What the move to `Defunct` takes out of a system: the process map's strong
/// reference to it and its reaper feeds.
type Released = (
    Option<Arc<ActorSystem>>,
    HashMap<tokio::runtime::Id, ReaperSlot>,
);

/// Counts one shutdown in flight (see `ActorSystem::registration_gate`):
/// entered with the phase change out of `Active`, and left when the shutdown
/// ends, completed or cut short. A completed one records that the system has
/// completed a shutdown since it was last `Active`; the shutdown that lowers
/// the count to zero stores `Defunct` when that is recorded, whichever order
/// the calls ended in. Registrations are refused from the first entry after
/// the system was last `Active` and every walk snapshots after its own entry,
/// so a completed walk has stopped every root registered before that. Only
/// cut-short shutdowns leave the system refusing registrations until a
/// shutdown of the system has completed (an earlier one counts).
pub(super) struct ShutdownInFlight<S: std::ops::Deref<Target = ActorSystem>> {
    system: S,
    pub(super) completed: bool,
}

impl<S: std::ops::Deref<Target = ActorSystem>> ShutdownInFlight<S> {
    pub(super) fn enter(system: S) -> Self {
        {
            let mut in_flight = system
                .registration_gate
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *in_flight += 1;
            system
                .phase
                .store(SystemPhase::ShuttingDown.as_u8(), Ordering::Release);
        }
        Self {
            system,
            completed: false,
        }
    }
}

impl<S: std::ops::Deref<Target = ActorSystem>> Drop for ShutdownInFlight<S> {
    fn drop(&mut self) {
        // What the move to `Defunct` lets go of, dropped after the gate is
        // released: the map's strong reference to the system and the reaper
        // feeds (each reaper fires what it holds, then ends once the last
        // clone of its feed is gone; the next feed starts a new one).
        let mut released: Option<Released> = None;
        {
            let mut in_flight = self
                .system
                .registration_gate
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *in_flight -= 1;
            if self.completed {
                self.system
                    .completed_since_active
                    .store(true, Ordering::Relaxed);
            }
            if *in_flight == 0 && self.system.completed_since_active.load(Ordering::Relaxed) {
                // No sweep of this system is in flight any more: the deadline
                // it held for them goes with the move to `Defunct`, under the
                // same lock, so a shutdown after a reactivation folds only its
                // own.
                self.system.sweep_deadline.send_replace(None);
                self.system
                    .phase
                    .store(SystemPhase::Defunct.as_u8(), Ordering::Release);
                // The map's hold goes in the same section, so the slot is
                // strong exactly until `Defunct` is stored.
                let slot = self.system.demote_slot();
                #[cfg(test)]
                {
                    let hook = self
                        .system
                        .after_demotion
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                let reapers = std::mem::take(
                    &mut *self
                        .system
                        .reaper
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
                released = Some((slot, reapers));
            }
        }
        drop(released);
    }
}

/// One target of the concurrent force sweep: its id, the snapshot of the
/// incarnation it is (a root's from the start of the shutdown, a non-root
/// leftover's from the listing that found it), and whether this shutdown had
/// already raised `Kill` on it.
struct SweepTarget {
    id: ActorId,
    snapshot: RootSnapshot,
    kill_raised: bool,
}

/// Snapshot of a root actor taken at the start of shutdown: independent of
/// the registry entry (the `status_rx` clone stays valid even after
/// `RegistryGuard` removes the entry), and exact to one incarnation (its
/// status channel is that incarnation's own).
#[derive(Clone)]
struct RootSnapshot {
    id: ActorId,
    registration_seq: u64,
    status_rx: StatusRx,
    kill_grace: Duration,
    /// The incarnation's abort record: a leftover whose registry entry is
    /// already freed is judged by the abort the reaper issues against it.
    record: IncarnationRef,
}

/// The reason a terminal plane carries: the one written, or `Kill` for a
/// plane that closed without one. Meaningful only once the plane is
/// terminal.
pub(crate) fn terminal_reason(rx: &StatusRx) -> StopReason {
    rx.reason_or_kill()
}

/// Waits, without a bound, until the plane is terminal and returns its
/// reason.
pub(crate) async fn wait_reason(rx: &mut StatusRx) -> StopReason {
    loop {
        if rx.status() == ActorStatus::Stopped {
            return rx.reason_or_kill();
        }
        if rx.changed().await.is_err() {
            return rx.reason_or_kill();
        }
    }
}

/// Waits, without a bound, until the plane is closed: every sender of it is
/// gone. An actor's senders are held by its task, its registry guard and its
/// timer and stream forwarders, so the plane closes once the actor's task has
/// dropped everything it owned and let go of the plane. That comes after
/// `Stopped` is published and after whatever runs between the publication and
/// the task's return (the end log line, with the `tracing` feature), and on
/// an abort with the task's drop. A version the plane carries does not end
/// the wait.
pub(crate) async fn wait_closed(rx: &mut StatusRx) {
    loop {
        if rx.has_changed().is_err() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// A final sweep's wait on one leftover: until its plane is terminal
/// (`Stopped` published, or closed) or the deadline passes. The deadline is
/// the earliest of `deadline` and every value the system's sweep deadline
/// takes while the wait runs, so a sweep that starts later with an earlier
/// deadline shortens it. Returns whether the plane was found terminal.
pub(super) async fn wait_end_by_the_system_deadline(
    rx: &mut StatusRx,
    system: &mut watch::Receiver<Option<Instant>>,
    deadline: Instant,
) -> bool {
    let mut deadline = deadline;
    let mut system_open = true;
    loop {
        if let Some(earlier) = *system.borrow_and_update() {
            deadline = deadline.min(earlier);
        }
        if read_status(rx) == ActorStatus::Stopped {
            return true;
        }
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return true;
                }
            }
            changed = system.changed(), if system_open => {
                if changed.is_err() {
                    system_open = false;
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                return read_status(rx) == ActorStatus::Stopped;
            }
        }
    }
}

/// The one classifier of a final-sweep leftover whose plane is terminal
/// (`Stopped` published, or closed): a status other than `Stopped` (its task
/// ended without publishing it) reads `Aborted`; `Stopped` reads `Killed`
/// when this shutdown's force took a `Kill` on it (`raised`), or when its
/// end became final at or after `deadline` (or carries no instant), and
/// `Graceful` when it became final before. The reason is the one the plane
/// carries. Every terminal leftover goes through here, whichever path
/// produced its report, so one end reads one way whichever call reports it.
pub(super) fn leftover_outcome(rx: &StatusRx, raised: bool, deadline: Instant) -> StopOutcome {
    let value = rx.borrow();
    if value.status != ActorStatus::Stopped {
        return StopOutcome::Aborted;
    }
    if raised || value.final_at.is_none_or(|at| at >= deadline) {
        StopOutcome::Killed
    } else {
        StopOutcome::Graceful
    }
}

/// The [`StopReport`] of a final-sweep leftover whose plane is terminal:
/// [`leftover_outcome`] and the reason the plane carries.
pub(super) fn leftover_report(rx: &StatusRx, raised: bool, deadline: Instant) -> StopReport {
    StopReport {
        outcome: leftover_outcome(rx, raised, deadline),
        reason: Some(terminal_reason(rx)),
    }
}

/// The [`StopReport`] of a terminal wait: the outcome from
/// [`terminal_outcome`] and the reason the plane carries.
pub(crate) fn terminal_report(rx: &StatusRx, requested: StopOutcome) -> StopReport {
    StopReport {
        outcome: terminal_outcome(rx, requested),
        reason: Some(terminal_reason(rx)),
    }
}

/// The report of an actor that reached no terminal status within the
/// call's last bound.
pub(super) fn unresponsive() -> StopReport {
    StopReport {
        outcome: StopOutcome::Unresponsive,
        reason: None,
    }
}

/// Waits, bounded by `bound`, for `status_rx` to reach a terminal state:
/// `ActorStatus::Stopped`, or the sender dropped without ever writing it (an
/// aborted task - terminal too, matching `ActorHandle::wait_stopped`).
/// Returns `true` if terminal was observed within `bound`.
pub(crate) async fn wait_terminal(status_rx: &mut StatusRx, bound: Duration) -> bool {
    tokio::time::timeout(bounded_wait(bound), async {
        while status_rx.status() != ActorStatus::Stopped {
            if status_rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await
    .is_ok()
}

/// The one classifier of a terminal wait. `requested` is the outcome the
/// call asked for (`Graceful` for a stop that raised a plain request, `Killed`
/// once a `Kill` was raised): it holds when the actor published `Stopped`
/// itself, and a status sender closed without that write means the task was
/// aborted, by the scheduled backstop or by the caller, and is `Aborted`.
/// Every terminal wait of `stop_within`, the shutdown ladder and the force
/// stops answers through here, so one condition gets one verdict on every
/// path, whichever of the deadlines fired first.
pub(crate) fn terminal_outcome(status_rx: &StatusRx, requested: StopOutcome) -> StopOutcome {
    if status_rx.status() == ActorStatus::Stopped {
        requested
    } else {
        StopOutcome::Aborted
    }
}

/// Waits until `deadline` for `status_rx` to reach a terminal state, as
/// [`wait_terminal`] does for a duration. Returns `true` if terminal was
/// observed first.
pub(crate) async fn wait_terminal_until(status_rx: &mut StatusRx, deadline: Instant) -> bool {
    tokio::time::timeout_at(deadline, async {
        while status_rx.borrow().status != ActorStatus::Stopped {
            if status_rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await
    .is_ok()
}
