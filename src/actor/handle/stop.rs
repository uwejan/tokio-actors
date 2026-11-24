//! Stop requests: `stop`, the bounded `stop_within` and the task that carries its ladder.

use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::time;
use tokio::time::Instant;

use super::{emit_stop_requested, ActorHandle};
use crate::actor::incarnation::{RaiseResult, StatusRx, StopLane};
use crate::actor::panic::{contain_waker_panic, send_contained};
use crate::actor::runtime::saturating_deadline;
use crate::actor::supervision::model::{
    SystemAction, SystemEnd, SystemLadder, SystemSeen, SystemStep,
};
use crate::actor::supervision::rules::system_ladder_step;
use crate::actor::supervision::{ABORT_SETTLE, DEFAULT_KILL_GRACE};
use crate::actor::Actor;
use crate::error::SendError;
use crate::helper::{HelperKind, HelperOwner};
use crate::system::{
    raise_kill_declared, read_status, terminal_outcome, wait_reason, wait_terminal,
    wait_terminal_until, ActorSystem, KillKey, LossOwes, ReaperEntry, StopStep,
};
use crate::telemetry::StopOrigin;
use crate::types::{ActorId, ActorStatus, StopOutcome, StopReason};

impl<A: Actor> ActorHandle<A> {
    /// Signals the actor to stop.
    ///
    /// A stop request is read at the next turn boundary, ahead of any message
    /// queued before it. Unless `pre_stop` vetoes it (only `Graceful`, `Shutdown`
    /// and `ParentRequest` can be vetoed), the actor ends without handling those
    /// messages, and a queued `send` returns `AskError::ResponseDropped`.
    /// Erlang/OTP receives `gen_server:stop` as a system message in arrival
    /// order, so earlier messages are handled first. To get that order, send a
    /// message whose handler calls `ctx.self_handle().stop(..)`.
    ///
    /// Delivered through the stop lane: a synchronous signal that never waits
    /// for capacity, which the run loop observes ahead of everything else - the
    /// system channel and the user mailbox alike - so it is never delayed by a
    /// full mailbox or a backlog of supervision traffic. Like every signal on
    /// the lane, it is only ever observed at a turn boundary: the signal itself
    /// never interrupts a callback already in progress; with
    /// [`StopReason::Kill`], the abort described below cuts a callback still
    /// awaiting once the actor's kill grace has passed.
    ///
    /// Escalation only ever increases: calling this with a lower-severity
    /// `reason` (for example `Graceful` after a `Kill` already landed) has no
    /// effect, and calling it again with the SAME severity - most commonly a
    /// retry after [`pre_stop`](crate::actor::Actor::pre_stop) vetoed the
    /// first request - always lands and, unless the
    /// [`pre_stop`](crate::actor::Actor::pre_stop) in flight accepts or
    /// panics, is guaranteed a fresh
    /// [`pre_stop`](crate::actor::Actor::pre_stop) invocation, even though the
    /// requested reason did not change tier. A request that lands while an
    /// accepting `pre_stop` runs is not asked again: the actor ends with the
    /// strongest request that landed. Within
    /// one severity the pending request keeps the higher rank: a parent's
    /// request is kept over a later shutdown or graceful one, a shutdown the
    /// actor asked for itself over a later graceful one, and a pending
    /// failure over a later cancellation. Calling
    /// this several times in a row before the actor gets back to its
    /// `select!` coalesces into a single observation of the highest severity
    /// requested: the actor sees at most one turn-boundary check per turn,
    /// never one per call.
    ///
    /// With [`StopReason::Kill`] the system also schedules the abort of the
    /// actor's task: if the actor does not reach a turn boundary within its
    /// kill grace (120 ms unless its spawn builder set another), its task is
    /// aborted. The abort is scheduled when the `Kill` is accepted while the
    /// actor is still registered, and never depends on this future. Once the
    /// actor has left the registry, its one remaining await is the end of
    /// its already aborted timer and stream tasks, which waits on their
    /// drops (user code, outside the end's guarantees, as the note on timers
    /// and streams says), so its end never precedes theirs and no abort is
    /// scheduled. A handler that raises `Kill` on itself
    /// through `ctx.self_handle().stop(StopReason::Kill)` and then awaits for
    /// longer than its kill grace is aborted at that await.
    ///
    /// For `Kill`, `Ok` means the request is accepted and the actor will end;
    /// the call does not wait for the end, and [`is_alive`](Self::is_alive)
    /// can still read `true` for a moment. [`wait_stopped`](Self::wait_stopped)
    /// waits for the end and reads `Kill`.
    ///
    /// # Errors
    /// Returns `SendError::Closed` if the actor does not accept the
    /// request any more: its task has ended (an aborted one included), it has
    /// sealed its stop lane, or it has left its message loop for good (its
    /// `on_stopped` has returned, a `Kill` ended the loop, which skips that
    /// hook, or its start failed) and the request is not [`StopReason::Kill`].
    /// A request other than `Kill` made while it runs its final deliveries or
    /// `on_stopped` is accepted and has no effect, and a `Kill` made then ends
    /// the deliveries and becomes the final reason. Through this handle a
    /// `Kill` is
    /// accepted until the actor seals its stop lane, just before its queued
    /// messages and its value are dropped; after that it answers
    /// `SendError::Closed`. [`ActorSystem::stop`](crate::system::ActorSystem::stop)
    /// and [`ActorSystem::kill`](crate::system::ActorSystem::kill) answer
    /// `NotFound` once the actor has left the registry.
    ///
    /// # Stopping after a delay
    ///
    /// A stop that is due later is a task of the application's own: it
    /// sleeps on the runtime it owns and then stops, or sends to, the target
    /// by its handle or id
    /// ([`kill_by_id`](crate::system::ActorSystem::kill_by_id) for a kill).
    /// A send to a target that has ended fails with `SendError::Closed`
    /// and harms nothing.
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, SendError, StopReason,
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
    /// #[tokio::main(flavor = "current_thread", start_paused = true)]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let worker = Worker.spawn().await?;
    ///     let target = worker.clone();
    ///     tokio::spawn(async move {
    ///         tokio::time::sleep(Duration::from_secs(30)).await;
    ///         let _ = target.stop(StopReason::Graceful).await;
    ///     });
    ///     assert_eq!(worker.wait_stopped().await, StopReason::Graceful);
    ///     assert!(matches!(worker.notify(()).await, Err(SendError::Closed)));
    ///     Ok(())
    /// }
    /// ```
    pub async fn stop(&self, reason: StopReason) -> Result<(), SendError> {
        if matches!(reason, StopReason::Kill) {
            // The deadline, the raise and the backstop are one call: the
            // system's, or none when it is gone.
            let system = self.system();
            let raised = raise_kill_declared(
                system.as_deref(),
                KillKey::Id(&self.inner.id),
                &self.inner.status_rx,
                &self.inner.stop_lane,
                crate::system::KillFrom::Plain,
            );
            // The shared raise queued the request's event.
            if raised.result == RaiseResult::Refused {
                return Err(SendError::Closed);
            }
            return Ok(());
        }
        if crate::telemetry::engaged_for(self.inner.stop_lane.abort_record(), None) {
            return self.stop_noted(reason);
        }
        if self.raise_request(reason) == RaiseResult::Refused {
            return Err(SendError::Closed);
        }
        Ok(())
    }

    /// The raise of a request that is not a `Kill`, the one site of
    /// [`stop`](Self::stop) and its telemetry tail.
    #[inline(always)]
    fn raise_request(&self, reason: StopReason) -> RaiseResult {
        self.inner.stop_lane.try_raise(reason)
    }

    /// The tail of [`stop`](Self::stop) for a system that takes telemetry
    /// events or holds an identity: the request is raised as it is and the
    /// lane's answer is queued as an event.
    #[inline(never)]
    fn stop_noted(&self, reason: StopReason) -> Result<(), SendError> {
        let kept = reason.clone();
        let raised = self.raise_request(reason);
        emit_stop_requested(
            None,
            &self.inner.stop_lane,
            &kept,
            raised,
            StopOrigin::Requested,
            None,
        );
        if raised == RaiseResult::Refused {
            return Err(SendError::Closed);
        }
        Ok(())
    }

    /// Stops this actor, bounded by `within`, and reports how far the
    /// escalation ladder had to go to get there.
    ///
    /// This stops THIS incarnation only. If the target is a supervised
    /// child, its supervisor may restart it per its own restart policy the
    /// moment it reaches a terminal status - `within` bounds this call's
    /// wait, not the child's overall lifetime. To stop a supervised child for
    /// good, use
    /// [`ActorContext::terminate_child`](crate::actor::context::ActorContext::terminate_child)
    /// from the parent instead.
    ///
    /// Raises `reason` on the stop lane, exactly like [`stop`](Self::stop),
    /// then waits up to `within` for a terminal status - including when
    /// `reason` is already [`StopReason::Kill`]: the call waits up to
    /// `within` for the actor to reach its next turn boundary and observe
    /// the kill before its task is aborted, since `Kill`, like every other
    /// reason, cannot interrupt a callback already in progress; for that
    /// call `within` replaces the actor's kill grace. If `reason` was not
    /// already `Kill` and the deadline passes first, the system escalates
    /// to `Kill` and waits the actor's kill grace; if the actor still has not
    /// reached a terminal status by then, its task is aborted directly and
    /// the system waits a short settle time before giving up. The window
    /// counts from the first poll of the call.
    ///
    /// A handler that never yields back to the run loop defeats every bound
    /// here - `Kill` is only ever observed at a turn boundary, and an abort
    /// only takes effect at an `.await` - and the call reports it as
    /// [`StopOutcome::Unresponsive`] when the runtime it is made on has a
    /// thread the handler does not hold. On a `current_thread` runtime that
    /// also runs the handler, the call makes no progress until the handler
    /// returns.
    ///
    /// Like Erlang/OTP's `gen_server:stop/3`, the wait is bounded; where that
    /// call exits the caller with `timeout` and does not kill the process, this
    /// one escalates when the window passes, first to `Kill` and then to an
    /// abort of the task, as a supervisor does with a child whose shutdown time
    /// ran out (its shutdown signal, then `kill`).
    ///
    /// # Dropping the future
    /// The first poll of the call commits the system to every step of the
    /// ladder, whether or not this future is still polled. For a reason other
    /// than `Kill`, while the actor is registered, a task of the system on the
    /// runtime that made the call takes the `Kill` step at `within` and the
    /// abort after the kill grace. For `Kill`, the raise itself arms the abort
    /// (or the attach does, when the abort handle is not stored yet), which the
    /// actor's own runtime takes after `within`. Dropping the future
    /// ends only the caller's wait. The stop
    /// request already raised stays raised (a `pre_stop` may still veto a
    /// `Graceful` one until the ladder's `Kill`). If the runtime that made the
    /// call ends before the task does, the system takes the same two steps from
    /// the actor's own runtime at the same instants, unless the actor's runtime
    /// has no time driver (then only a `Kill` whose window has already passed
    /// is taken, and it aborts the target at once, as every `Kill` on such a
    /// runtime does) or the actor's abort handle is not attached yet (then
    /// the `Kill` is taken at once). A pending call with a reason other than
    /// `Kill` holds that task, a small heap allocation, until the call
    /// returns or, once the call is dropped, until its ladder or its target
    /// ends; a `Kill` request holds
    /// none. A call that is kept alive and never polled again
    /// releases it once its window has passed and its ladder has ended. No
    /// public operation reaches the next case: a handle reaches the system it
    /// was spawned in through the stop lane it holds, and that system is held
    /// by the actor until the actor has sealed its stop lane, so a request
    /// that lane still takes always finds its system. For a target with no
    /// system, there is no system to own the steps and no registry to arm:
    /// the ladder runs in this future, raises its `Kill` and has no abort
    /// handle, so it aborts nothing; dropping the future ends it.
    ///
    /// OTP parity: like the shutdown that `supervisor:terminate_child` runs
    /// inside the supervisor, the ladder goes on when the caller gives up
    /// waiting. Where OTP waits for the `'DOWN'` after its kill without
    /// limit, the system waits a short settle time and the call may report
    /// [`StopOutcome::Unresponsive`].
    ///
    /// OTP parity: `proc_lib:stop/3`, which `gen_server:stop/3` uses, runs the
    /// stop in the caller and, when its timeout passes, exits the caller with
    /// `timeout` without killing the target. Here, for a reason other than
    /// `Kill`, the system takes the `Kill` at the window and the abort after
    /// the kill grace (for `Kill`, the abort after the window), whether or not
    /// the caller goes on waiting: a declared departure.
    ///
    /// For a reason other than `Kill`, the call returns when the stop task has
    /// answered or as soon as the actor reaches a terminal status, whichever
    /// comes first: a graceful stop completes without the task being polled. A
    /// `Kill` call has no stop task and returns when the actor reaches a
    /// terminal status, or reports `Unresponsive` after `within` and the settle
    /// time.
    ///
    /// A zero window acts at once: `Duration::ZERO` raises the `Kill` inside
    /// the call, right after the request, so an actor that has not run since
    /// the request skips `pre_stop` and `on_stopped` (on a multi-thread
    /// runtime an actor already running can take the request first).
    ///
    /// # Panics
    /// Panics when no Tokio runtime is current on the calling thread, or the
    /// current one was built without the time driver (`Builder::enable_time`
    /// or `enable_all`), before the call changes anything; that is the only
    /// runtime the check refuses, and a calling runtime that is shutting down
    /// or has ended passes it; an actor that no
    /// longer takes the request is answered with `Err` first, on any
    /// runtime. The call needs a
    /// runtime that is driven, not only entered: for a reason other than
    /// `Kill`, its escalation steps run on the calling runtime, so a caller
    /// polled by a foreign executor under a runtime nothing drives completes
    /// only when the actor ends without the ladder (by itself, or from the
    /// `Kill` that a zero window raises inside the call). A `Kill` call
    /// completes when the actor ends, from the `Kill` or from the abort the
    /// system armed on the actor's own runtime; its `Unresponsive` answer
    /// needs the calling runtime to be driven. If the
    /// calling runtime ends, the system hands the steps of a call with a reason
    /// other than `Kill` to the actor's own runtime (with the exceptions
    /// above), which ends the actor; a call of either kind that is polled by a
    /// foreign executor with the ended runtime still entered panics at its next
    /// timed wait, unless the actor has reached a terminal status by then (a
    /// poll with no runtime context panics at a non-`Kill` call's closing wait
    /// whatever the actor's status).
    /// With `panic = "abort"` the entry cannot tell whether the runtime has the
    /// time driver, so a call on a runtime without it ends the process at the
    /// first timer it uses.
    ///
    /// # Calling oneself
    /// Called on oneself from one of the actor's own callbacks the call is not
    /// refused, and it does not return to the callback: the callback waits for
    /// the actor's end, which the waiting callback holds up. Only the ladder's
    /// `Kill` and the abort the system arms end the actor, and that drops the
    /// callback and the call with it.
    ///
    /// # Errors
    /// Returns `SendError::Closed` if the actor does not accept the
    /// request any more, the same case [`stop`](Self::stop) reports.
    pub async fn stop_within(
        &self,
        reason: StopReason,
        within: Duration,
    ) -> Result<StopOutcome, SendError> {
        // An actor that does not take the request is answered first, as
        // `stop` answers it: that needs no timer. The timer check comes
        // next, and the raise, which changes state, after it.
        if self.inner.stop_lane.refuses(&reason) {
            emit_stop_requested(
                None,
                &self.inner.stop_lane,
                &reason,
                RaiseResult::Refused,
                StopOrigin::Requested,
                None,
            );
            return Err(SendError::Closed);
        }
        crate::time_probe::require_caller_timers("ActorHandle::stop_within");
        let caller_runtime = Handle::current();
        // Every deadline of the call counts from this instant.
        let call = Instant::now();
        let mut rx = self.inner.status_rx.clone();
        let is_kill = matches!(reason, StopReason::Kill);
        // The incarnation's own kill grace, read while its registry entry
        // still holds it: the entry is freed before the actor publishes
        // `Stopped`, and the Kill step's wait must hold after that. An actor
        // already out of the registry refuses every reason but `Kill` below.
        let system = self.system();
        let kill_grace = system
            .as_ref()
            .and_then(|system| system.kill_target_for(&self.inner.id, &self.inner.status_rx))
            .map_or(DEFAULT_KILL_GRACE, |(_, grace, _)| grace);
        let timing = (call, within, kill_grace);
        // A `Kill` queues its request event in the shared raise.
        let kept = (!is_kill
            && crate::telemetry::engaged_for(self.inner.stop_lane.abort_record(), None))
        .then(|| reason.clone());
        // A call made from inside a task spawns the stop task before it
        // raises the first request, so the actor that the request wakes
        // keeps the worker's next-task slot and runs on the worker that
        // raised it, and the stop task stays asleep until the call is over
        // or the window ends, and takes no step until the raise is decided.
        // A request the lane refuses ends the task before it takes any step.
        // A call made from outside every task has no such slot to keep: it
        // raises first, so the actor is the first to be picked up, and its
        // stop task watches the status plane itself. An actor already out of
        // its status plane refuses every request, so it gets no task.
        let in_task = tokio::task::try_id().is_some();
        let mut ladder = match &system {
            Some(system) if !is_kill && in_task && read_status(&rx) != ActorStatus::Stopped => {
                Some(spawn_ladder(
                    system,
                    &caller_runtime,
                    &self.inner.id,
                    &self.inner.stop_lane,
                    &rx,
                    (timing, in_task),
                    false,
                ))
            }
            _ => None,
        };
        // Decides the first request as refused if the call unwinds before it
        // decides: a panic inside the raise after the lane stored the request
        // counts as refused too (the raise has no such panic site outside
        // the tests' hooks).
        let first_request = FirstRequest(ladder.as_ref().map(|ladder| ladder.step.clone()));
        #[cfg(test)]
        if let Some(system) = &system {
            system.run_ladder_hook(crate::system::LadderPoint::StopBeforeRaise, &self.inner.id);
        }
        let raised = if is_kill {
            raise_kill_declared(
                system.as_deref(),
                KillKey::Id(&self.inner.id),
                &self.inner.status_rx,
                &self.inner.stop_lane,
                crate::system::KillFrom::Window(within),
            )
            .result
        } else {
            self.inner.stop_lane.try_raise(reason)
        };
        #[cfg(test)]
        if let Some(system) = &system {
            system.run_ladder_hook(crate::system::LadderPoint::StopRaised, &self.inner.id);
        }
        // The decision follows the raise with nothing awaited in between.
        if raised == RaiseResult::Refused {
            if let Some(step) = &first_request.0 {
                step.decide_refused();
            }
            #[cfg(test)]
            if let Some(system) = &system {
                system.run_ladder_hook(crate::system::LadderPoint::StopDecided, &self.inner.id);
            }
            if let Some(kept) = &kept {
                emit_stop_requested(
                    system.as_deref(),
                    &self.inner.stop_lane,
                    kept,
                    raised,
                    StopOrigin::Requested,
                    None,
                );
            }
            return Err(SendError::Closed);
        }
        let task_lost = first_request
            .0
            .as_ref()
            .is_some_and(|step| step.decide_accepted());
        #[cfg(test)]
        if let Some(system) = &system {
            system.run_ladder_hook(crate::system::LadderPoint::StopDecided, &self.inner.id);
        }
        if task_lost {
            // The stop task was lost before it could know the request was
            // accepted, so it handed nothing over: the caller hands over the
            // steps the task owed, before it awaits anything.
            if let (Some(system), Some(ladder)) = (&system, &ladder) {
                hand_off_steps(
                    system,
                    &self.inner.id,
                    &self.inner.stop_lane,
                    &rx,
                    &ladder.step,
                    (call, within),
                );
            }
        }
        if let Some(kept) = &kept {
            emit_stop_requested(
                system.as_deref(),
                &self.inner.stop_lane,
                kept,
                raised,
                StopOrigin::Requested,
                None,
            );
        }

        if is_kill {
            // The `Kill` and its backstop are the system's from the raise.
            if wait_terminal(&mut rx, within).await {
                return Ok(terminal_outcome(&rx, StopOutcome::Killed));
            }
            // The abort after the window is the system's backstop, armed above:
            // the caller only waits for the end and classifies it.
            if wait_terminal(&mut rx, ABORT_SETTLE).await {
                return Ok(terminal_outcome(&rx, StopOutcome::Killed));
            }
            return Ok(StopOutcome::Unresponsive);
        }

        let Some(system) = system else {
            // A target with no system has no helper slot and no registry:
            // the ladder runs in the caller's own future.
            let mut task = StopTask::new(
                None,
                self.inner.id.clone(),
                self.inner.stop_lane.clone(),
                rx,
                Arc::new(StopStep::accepted()),
                timing,
                None,
            );
            return Ok(task.run().await);
        };
        let Ladder {
            answer: mut answer_rx,
            step,
        } = ladder.take().unwrap_or_else(|| {
            spawn_ladder(
                &system,
                &caller_runtime,
                &self.inner.id,
                &self.inner.stop_lane,
                &rx,
                (timing, in_task),
                true,
            )
        });

        // A window that has passed at the call leaves nothing to wait for:
        // the `Kill` is raised here, in the call that raised the request,
        // not one scheduler hop later in the stop task, so an actor that has
        // not run since the request reads the `Kill` first.
        if saturating_deadline(call, within) <= call {
            raise_kill_declared(
                Some(&system),
                KillKey::Id(&self.inner.id),
                &rx,
                &self.inner.stop_lane,
                crate::system::KillFrom::Step(&step),
            );
        }

        // The caller only waits, for the task's answer and for the actor's
        // end together: whichever comes first decides, and both read the
        // same step cell, so one condition gets one outcome.
        let mut plane = rx.clone();
        let outcome = tokio::select! {
            biased;
            answered = &mut answer_rx => match answered {
                Ok(outcome) => outcome,
                Err(_) => {
                    closed_answer_outcome(
                        &system,
                        &self.inner.id,
                        &mut rx,
                        &step,
                        timing,
                    )
                    .await
                }
            },
            _ = wait_reason(&mut plane) => terminal_outcome(&rx, step.requested().await),
        };
        Ok(outcome)
    }
}

/// The instant at which the whole ladder of a call made at `call` is over:
/// the window, the kill grace and the settle wait, each added with its own
/// cap so that no sum is capped as a whole.
fn ladder_end(call: Instant, within: Duration, kill_grace: Duration) -> Instant {
    saturating_deadline(
        saturating_deadline(saturating_deadline(call, within), kill_grace),
        ABORT_SETTLE,
    )
}

/// The outcome of a bounded stop whose stop task ended without answering,
/// classified from the step cell and the status plane: the plane reads
/// `Stopped` gives what that step asked for, a plane closed without it
/// gives `Aborted`, and a plane that is not terminal when the ladder's own
/// bound has passed gives `Unresponsive`.
async fn closed_answer_outcome(
    system: &ActorSystem,
    id: &ActorId,
    rx: &mut StatusRx,
    step: &StopStep,
    (call, within, kill_grace): (Instant, Duration, Duration),
) -> StopOutcome {
    #[cfg(test)]
    system.run_ladder_hook(crate::system::LadderPoint::StopClosedSeen, id);
    #[cfg(not(test))]
    let _ = (system, id);
    if wait_terminal_until(rx, ladder_end(call, within, kill_grace)).await {
        terminal_outcome(rx, step.requested().await)
    } else {
        StopOutcome::Unresponsive
    }
}

/// The task of one bounded stop, as its caller holds it: the receiver of the
/// task's answer and the step cell both read.
struct Ladder {
    answer: oneshot::Receiver<StopOutcome>,
    step: Arc<StopStep>,
}

/// The caller's hold on the decision of its first request: dropping it
/// decides the request as refused when the call unwound before deciding.
struct FirstRequest(Option<Arc<StopStep>>);

impl Drop for FirstRequest {
    fn drop(&mut self) {
        if let Some(step) = &self.0 {
            step.decide_refused();
        }
    }
}

/// Spawns the stop task of a call on `rt` through the system's helper slot.
/// With `accepted` false the task takes no step until the caller decides the
/// call's first request.
/// The task holds the stop lane, the status receiver, the system and plain
/// values, never an `ActorHandle` and never a mailbox sender.
fn spawn_ladder(
    system: &Arc<ActorSystem>,
    rt: &Handle,
    id: &ActorId,
    lane: &StopLane,
    rx: &StatusRx,
    (timing, deferred): ((Instant, Duration, Duration), bool),
    accepted: bool,
) -> Ladder {
    let step = Arc::new(if accepted {
        StopStep::accepted()
    } else {
        StopStep::default()
    });
    let (answer_tx, answer_rx) = oneshot::channel();
    let owner = HelperOwner::new(id.clone(), Some(lane.abort_record().clone()));
    drop(
        system.spawn_helper(rt, HelperKind::StopLadder, Some(owner), {
            let task_system = system.clone();
            let (id, lane, rx, step) = (id.clone(), lane.clone(), rx.clone(), step.clone());
            move |end| {
                // Built here, outside the task's future, so a task that is
                // dropped before its first poll is covered too.
                let mut task = StopTask::new(
                    Some(task_system),
                    id,
                    lane,
                    rx,
                    step,
                    timing,
                    Some(answer_tx),
                );
                task.deferred = deferred;
                end.observe(async move {
                    let outcome = task.run().await;
                    task.answer(outcome);
                    #[cfg(test)]
                    task.hook(crate::system::LadderPoint::StopAnswered);
                })
            }
        }),
    );
    Ladder {
        answer: answer_rx,
        step,
    }
}

/// The ladder of one bounded stop whose first request is raised. It is the
/// stop task of the system and the guard of that task in one value: the
/// task takes the steps, answers the caller through `answer`, and when the
/// task is lost before its last wait, by a panic or because its runtime
/// ended, dropping the value hands the steps that are still owed to the
/// reaper of the actor's own runtime, before the answer channel closes.
pub(super) struct StopTask {
    system: Option<Arc<ActorSystem>>,
    id: ActorId,
    lane: StopLane,
    rx: StatusRx,
    step: Arc<StopStep>,
    /// The instant of the call, the window and the kill grace.
    call: Instant,
    within: Duration,
    kill_grace: Duration,
    /// Set once the ladder has run to its end.
    pub(super) done: bool,
    /// Whether the task sleeps until the window ends or the call goes away
    /// before it reads the status plane.
    deferred: bool,
    answer: Option<oneshot::Sender<StopOutcome>>,
}

impl StopTask {
    pub(super) fn new(
        system: Option<Arc<ActorSystem>>,
        id: ActorId,
        lane: StopLane,
        rx: StatusRx,
        step: Arc<StopStep>,
        (call, within, kill_grace): (Instant, Duration, Duration),
        answer: Option<oneshot::Sender<StopOutcome>>,
    ) -> Self {
        Self {
            system,
            id,
            lane,
            rx,
            step,
            call,
            within,
            kill_grace,
            done: false,
            deferred: false,
            answer,
        }
    }

    #[cfg(test)]
    fn hook(&self, point: crate::system::LadderPoint) {
        if let Some(system) = &self.system {
            system.run_ladder_hook(point, &self.id);
        }
    }

    /// Arms the `StopKillStored` point to run inside the next raise on this
    /// thread, once the step cell counted the raiser and before the raise.
    #[cfg(test)]
    fn arm_stored_hook(&self) {
        let (system, id) = (self.system.clone(), self.id.clone());
        crate::system::run_once_before_the_next_kill_raise(move || {
            if let Some(system) = system {
                system.run_ladder_hook(crate::system::LadderPoint::StopKillStored, &id);
            }
        });
    }

    /// Waits up to the window for a terminal status, raises `Kill` and arms
    /// its backstop, waits the kill grace, then aborts and settles, each step
    /// as [`system_ladder_step`] names it. A `Kill` the lane refuses belongs
    /// to an actor already past its last read of it: it ends with its own
    /// reason, and the outcome reads `Graceful`. The abort is issued when the
    /// registry still holds the task's abort handle.
    async fn run(&mut self) -> StopOutcome {
        #[cfg(test)]
        self.hook(crate::system::LadderPoint::StopHelperStart);
        let window_end = saturating_deadline(self.call, self.within);
        // A deferred task needs nothing of the call before the window ends:
        // the caller reads the status plane itself and returns, or is
        // dropped, which closes the answer. The task sleeps until the first
        // of the two and reads the plane then.
        if let Some(answer) = self.answer.as_mut().filter(|_| self.deferred) {
            tokio::select! {
                biased;
                () = answer.closed() => {}
                () = time::sleep_until(window_end) => {}
            }
        }
        let ladder = SystemLadder::Bounded {
            window_end: window_end.into_std(),
            grace: self.kill_grace,
        };
        let accepted = self.step.first_request_accepted().await;
        let mut now = Instant::now().into_std();
        let begin = SystemSeen::Begin { accepted };
        let (mut step, mut action) = system_ladder_step(ladder, SystemStep::Start, begin, now);
        // The step the last decision was made at, and what a `Stopped` the
        // actor publishes reads as, read before the abort.
        let (mut from, mut requested) = (SystemStep::Start, None);
        loop {
            let seen = match action {
                SystemAction::WaitUntil(until) => {
                    let in_window = matches!(step, SystemStep::Window { .. });
                    let ended = if in_window {
                        wait_terminal_until(&mut self.rx, Instant::from_std(until)).await
                    } else {
                        wait_terminal(&mut self.rx, until.saturating_duration_since(now)).await
                    };
                    if ended {
                        SystemSeen::Ended
                    } else {
                        #[cfg(test)]
                        if in_window {
                            self.hook(crate::system::LadderPoint::StopWithinKill);
                        }
                        // A call whose window had passed at the call took the
                        // `Kill` step itself.
                        SystemSeen::Passed {
                            kill_started: in_window && self.step.kill_started(),
                        }
                    }
                }
                SystemAction::Kill { raise } => {
                    if raise {
                        #[cfg(test)]
                        self.arm_stored_hook();
                        raise_kill_declared(
                            self.system.as_deref(),
                            KillKey::Id(&self.id),
                            &self.rx,
                            &self.lane,
                            crate::system::KillFrom::Step(&self.step),
                        );
                    }
                    #[cfg(test)]
                    self.hook(crate::system::LadderPoint::StopAfterKill);
                    SystemSeen::Taken { raised: raise }
                }
                SystemAction::Abort => {
                    requested = Some(self.step.requested().await);
                    if let Some(system) = self.system.as_deref() {
                        if let Some((Some(abort), _, _)) =
                            system.kill_target_for(&self.id, &self.rx)
                        {
                            self.lane.abort_record().abort(&abort);
                        }
                    }
                    SystemSeen::Taken { raised: false }
                }
                SystemAction::Finish(end) => {
                    // An outcome the window gave owes no step from here on.
                    if matches!(from, SystemStep::Start | SystemStep::Window { .. }) {
                        self.done = true;
                    }
                    let outcome = match end {
                        SystemEnd::Terminal { .. } => {
                            let requested = match requested {
                                Some(requested) => requested,
                                None => self.step.requested().await,
                            };
                            terminal_outcome(&self.rx, requested)
                        }
                        SystemEnd::Refused => StopOutcome::Graceful,
                        SystemEnd::Unresponsive => StopOutcome::Unresponsive,
                        SystemEnd::Sweep { .. } => unreachable!("a bounded stop has no sweep"),
                    };
                    self.done = true;
                    return outcome;
                }
                SystemAction::Request | SystemAction::WaitFloor { .. } | SystemAction::None => {
                    unreachable!("a bounded stop takes no such step")
                }
            };
            now = Instant::now().into_std();
            from = step;
            (step, action) = system_ladder_step(ladder, from, seen, now);
        }
    }

    /// Sends the outcome to the caller, if it still waits.
    pub(super) fn answer(&mut self, outcome: StopOutcome) {
        self.done = true;
        if let Some(answer) = self.answer.take() {
            let _ = send_contained(answer, outcome);
        }
    }

    /// The steps a lost task still owed (see [`hand_off_steps`]).
    fn carry_on(&self) {
        let Some(system) = &self.system else { return };
        hand_off_steps(
            system,
            &self.id,
            &self.lane,
            &self.rx,
            &self.step,
            (self.call, self.within),
        );
    }
}

/// The steps a lost stop task still owed: the `Kill` at the end of the window
/// and the abort after the kill grace, both counted from the call's first poll, as
/// one entry of the reaper on the actor's own runtime. A target whose
/// abort handle is not attached yet gets its `Kill` at once, and the
/// attach arms the abort. In that window the `Kill` comes when the task
/// is lost, which can be before the end of the window the call was given,
/// and a `pre_stop` veto of the earlier request is cut short by that
/// much. A target out of the registry is past the point where either
/// step could matter. A target on a runtime without the time driver gets
/// no entry, which that runtime's reaper could not hold: its steps are
/// not taken, except a `Kill` whose window has already passed.
fn hand_off_steps(
    system: &Arc<ActorSystem>,
    id: &ActorId,
    lane: &StopLane,
    rx: &StatusRx,
    step: &Arc<StopStep>,
    (call, within): (Instant, Duration),
) {
    let Some((abort, grace, runtime)) = system.kill_target_for(id, rx) else {
        return;
    };
    match abort {
        Some(abort) => {
            let kill_at = saturating_deadline(call, within);
            if !crate::time_probe::runtime_has_timers(&runtime) {
                // The reaper of a runtime without the time driver cannot
                // hold an entry, and the entry's drop would abort the
                // target before its window: the lost step does nothing,
                // unless the window has already passed.
                // The `Kill` is due, and the reaper of such a runtime ends
                // the target at once, as it does for every `Kill` there.
                if Instant::now() >= kill_at {
                    raise_kill_declared(
                        Some(system),
                        KillKey::Id(id),
                        rx,
                        lane,
                        crate::system::KillFrom::Step(step),
                    );
                }
                return;
            }
            let entry = ReaperEntry::two_stage(
                kill_at,
                lane.clone(),
                saturating_deadline(kill_at, grace),
                abort,
            )
            .with_step(step.clone());
            system.feed_reaper(&runtime, entry);
        }
        None => {
            raise_kill_declared(
                Some(system),
                KillKey::Id(id),
                rx,
                lane,
                crate::system::KillFrom::Step(step),
            );
        }
    }
}

impl Drop for StopTask {
    fn drop(&mut self) {
        if !self.done {
            // A first request not decided yet leaves the loss to the caller,
            // which hands the steps over when it finds the request accepted.
            match self.step.lose_before_the_last_wait() {
                LossOwes::Steps => {
                    self.carry_on();
                    #[cfg(test)]
                    self.hook(crate::system::LadderPoint::StopHelperLost);
                }
                LossOwes::Nothing =>
                {
                    #[cfg(test)]
                    if !self.step.refused() {
                        self.hook(crate::system::LadderPoint::StopHelperLost);
                    }
                }
            }
        }
        // The caller sees the closed answer only once the steps are handed
        // over, so the answer sender is dropped last. Dropping it wakes the
        // caller of the stop, whose waker belongs to the caller's task, and
        // this drop can run while the task unwinds: a panic of that waker
        // stays out of it.
        if let Some(answer) = self.answer.take() {
            contain_waker_panic("stop answer", Some(self.id.as_who()), move || drop(answer));
        }
    }
}
