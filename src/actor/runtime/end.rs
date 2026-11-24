//! The end sequence of an actor's task, one function per step, in order.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::{
    end_value, final_drain, keep_first_of, kill_stored, monitor_drain, stop_all_children,
    AbortReason, CaughtDrop, FinalOutcome, HeldAnswer, HookOutcome, Owner, QueueSlot, ReplyDrain,
    SlotHold, TaskMarker, Teardown,
};
use crate::actor::context::ActorContext;
use crate::actor::incarnation::turn_clock::{push_run, push_turn, TurnGuard};
use crate::actor::incarnation::{
    IncarnationRef, LaneRx, StatusTx, StatusValue, SubtreeHold, SystemPlane,
};
use crate::actor::mailbox::MailRx;
use crate::actor::panic::{
    catch_hook_t, contain_waker_panic, payload_into_string, send_contained, AnswerHold,
};
#[cfg(test)]
use crate::actor::runtime::end_hooks;
use crate::actor::supervision::SupervisionState;
use crate::actor::{Actor, ActorEnvelope};
use crate::error::ActorError;
use crate::system::{ActorSystem, RegistryGuard};
use crate::telemetry::{reports, EventKind, HookKind, HotCache, Identity, TelemetryEvent};
use crate::types::{ActorId, ActorStatus, StopReason};

/// What the message loop leaves to the end: the loop's copy of the handler
/// list, its hold on the waker slot, the run still open when the loop ended
/// (its takes and when it started), and the reply drain the stop's deliveries
/// go on with.
pub(super) struct LoopEnd {
    pub(super) hot: HotCache,
    pub(super) lane_slot: SlotHold,
    pub(super) run: Option<(u32, std::time::Instant)>,
    pub(super) drain: ReplyDrain,
}

/// The identity fields the end's log lines carry.
#[cfg(feature = "tracing")]
type EndFields = crate::telemetry::LogFields<'static>;

/// Without the `tracing` feature the end writes no log line and carries no
/// fields.
#[cfg(not(feature = "tracing"))]
struct EndFields;

/// What the end of the actor's task works on: every parameter of the run
/// loop and every local the end still needs, moved in when the message loop
/// is over (or the start failed). The end's future holds it, once. The fields
/// are declared in the order an abort or an unwind while the end is suspended
/// drops them, first to last, after the end's own locals, which is
/// the order `run_actor` documents for those paths: the body's locals (the
/// reason, the held answer, the teardown panic, the init result), then the
/// parameters last to first, `ack`, `registry_guard`, `turn_clock`,
/// `stop_rx`, `system_rx`, `_abort_reason`, `priority_slot`, `mailbox_slot`,
/// `ctx_slot`, `actor_slot`, `status`, `_subtree`. Whatever the end already
/// released is skipped. On return the fields left, `turn_clock`, `status` and
/// `_subtree`, drop in that order, as the run loop's return dropped them.
pub(super) struct End<A: Actor> {
    // What the message loop left, `None` when the loop never ran (a failed
    // or cut initialization).
    pub(super) looped: Option<LoopEnd>,
    pub(super) stop_reason: StopReason,
    pub(super) held: HeldAnswer<A::Response>,
    pub(super) teardown_panic: Option<String>,
    pub(super) init_result: Result<(), ActorError>,
    pub(super) ack: AnswerHold<Result<(), ActorError>>,
    pub(super) registry_guard: Option<RegistryGuard>,
    pub(super) turn_clock: TurnGuard,
    // Held until the seal drops it.
    pub(super) stop_rx: Option<LaneRx>,
    // Held until the inputs narrow.
    pub(super) system_rx: Option<SystemPlane>,
    pub(super) _abort_reason: AbortReason,
    pub(super) priority_slot: QueueSlot<mpsc::Receiver<ActorEnvelope<A>>>,
    pub(super) mailbox_slot: QueueSlot<MailRx<ActorEnvelope<A>>>,
    pub(super) ctx_slot: CaughtDrop<ActorContext<A>>,
    pub(super) actor_slot: CaughtDrop<A>,
    pub(super) status: StatusTx,
    pub(super) _subtree: Arc<SubtreeHold>,
}

/// The stop lane's receiver, held until the seal.
fn held_lane(stop_rx: &mut Option<LaneRx>) -> &mut LaneRx {
    stop_rx.as_mut().expect("the lane is held until the seal")
}

impl<A: Actor> End<A> {
    /// The end of the actor's task, one step after the other in the order
    /// each step's comment fixes. Only an actor whose message loop ran closes
    /// it, enters `Stopping`, delivers what it still owes and runs
    /// `on_stopped`. Returns the reason the task ends with.
    // Not an `async fn`: an `async fn` keeps its parameter twice (the
    // unresumed state's copy and the body's moved-in copy), while the block
    // takes the end's inputs by value and works on them in place, so its
    // future holds them once.
    #[allow(clippy::manual_async_fn)]
    pub(super) fn end(mut self) -> impl Future<Output = StopReason> {
        async move {
            if let Some(LoopEnd {
                hot,
                lane_slot,
                run,
                mut drain,
            }) = self.looped.take()
            {
                self.close_loop(hot, lane_slot, run);
                #[cfg(test)]
                end_hooks::pause(end_hooks::LOOP_ENDED, self.ctx_slot.get_mut().actor_id()).await;

                // The final deliveries, then Phase 4: on_stopped (terminate/2
                // parity: runs on failures and panics, skipped only for Kill /
                // brutal_kill). A supervisor first delivers, once each, the child
                // deaths still pending when its message loop ended (rule: its
                // status reads `Stopping` in every one of them). A `Kill` that
                // arrives meanwhile ends the deliveries and skips `on_stopped`.
                let mut pending_exit: Option<StopReason> = None;
                if !matches!(self.stop_reason, StopReason::Kill) {
                    self.enter_stopping();
                    pending_exit = self.final_reports().await;
                    #[cfg(test)]
                    end_hooks::pause(end_hooks::FINAL_DONE, self.ctx_slot.get_mut().actor_id())
                        .await;
                    self.owed_notices(&mut drain, &mut pending_exit).await;
                }
                #[cfg(test)]
                end_hooks::pause(end_hooks::STAGE_DONE, self.ctx_slot.get_mut().actor_id()).await;
                self.last_hook(pending_exit).await;
            }

            self.last_kill_read();
            self.narrow_inputs();
            self.close_queues();
            let owner = self.ctx_slot.get_mut().owner();
            self.stop_children(&owner).await;
            self.free_name(&owner);

            // Every timer and stream forwarder is ended and awaited first, each
            // future dropped with the stream, message or factory it owns; a panic
            // in the drop of a timer or stream still registered is part of the
            // end. From the mailbox's close (above) on, every forwarder releases
            // what it owns when it next runs, and this abort drops what one that
            // has not run still owns; either way a drop panic is classified by
            // registration. A panic of a poll, a drop made before the forwarder
            // saw the close, and any panic of a timer or stream cancelled during
            // the run belong to the run (an aborted forwarder is never polled
            // again), whichever task runs first.
            // A `Kill` raised meanwhile is still accepted and read by the seal
            // below. This is the last await of the task: from the seal on, the end
            // runs without a yield, so an abort that lands while the queued
            // messages or the value drop cannot change how the task ends.
            #[cfg(test)]
            end_hooks::pause(end_hooks::TEARDOWN_DONE, &owner).await;
            let end_panic = self.ctx_slot.get_mut().end_forwarders().await;

            self.late_panics(end_panic, &owner);
            let kill_cause = self.seal();
            self.answer_held(&owner);
            let (cause_reason, fields) = self.write_reason(kill_cause);
            self.answer_ack(&owner, &fields);
            // Below this point `stop_reason` still says `Kill` for the
            // classifications that read the kind of the end (the init ack, the
            // crash report), and the cause, when a `Kill` carried one, is the
            // value the plane, the end event, the log and the task's return carry.
            let crash = self.drop_owned(cause_reason.as_ref(), &owner, &fields);
            // `Stopped` is published once every drop of what the actor owned is
            // done: a waiter of `wait_stopped` finds nothing of the actor's left.
            publish_stopped(&self.status);
            self.report_end(crash, cause_reason.as_ref(), &owner, &fields);

            let stop_reason = std::mem::replace(&mut self.stop_reason, StopReason::Graceful);
            cause_reason.unwrap_or(stop_reason)
        }
    }

    /// Closes the message loop: the run still open when the loop exits ends
    /// before the stop sequence, which it must not wait for. Then the debug
    /// log keeps the reason the loop ended with, the loop's handler cache and
    /// waker slot are let go, and a message a callback asked to handle next is
    /// not handled once the loop has ended.
    fn close_loop(
        &mut self,
        hot: HotCache,
        lane_slot: SlotHold,
        run: Option<(u32, std::time::Instant)>,
    ) {
        if let Some((takes, turn_start)) = run {
            push_run(&lane_slot.0, takes, turn_start);
        }
        let ctx = self.ctx_slot.get_mut();
        // The log keeps the reason the loop ended with.
        if let Some(state) = ctx.debug_mut() {
            state.stopping(&self.stop_reason);
        }
        drop(hot);
        drop(lane_slot);
        ctx.drop_continuation();
    }

    /// Publishes `Stopping`, for any end but a `Kill`, before any stop hook
    /// runs: a reader of the status plane sees the stop before the first
    /// callback of the stop begins.
    fn enter_stopping(&mut self) {
        self.ctx_slot.get_mut().set_status(ActorStatus::Stopping);
    }

    /// A supervisor's final collection and delivery of the child reports
    /// still pending; any other actor yields once. A task woken by the
    /// `Stopping` publish is queued on this worker, and a callback that blocks
    /// the thread would starve it until the callback returns. One yield lets
    /// it run first, before the first callback of the stop: after the final
    /// collection for a supervisor, whose collection must see the deaths of
    /// the turn that ended its loop. A `Kill` that reached the supervisor
    /// makes the reason `Kill`. Returns the exit reason a failed delivery
    /// makes of the supervisor.
    async fn final_reports(&mut self) -> Option<StopReason> {
        let ctx = self.ctx_slot.get_mut();
        if ctx.supervision_ref().is_some() {
            // Boxed: only a supervisor's teardown pays for this state.
            match Box::pin(final_drain(
                self.actor_slot.get_mut(),
                ctx,
                held_lane(&mut self.stop_rx),
                &self.stop_reason,
                &mut self.teardown_panic,
            ))
            .await
            {
                FinalOutcome::Killed => {
                    self.stop_reason = StopReason::Kill;
                    None
                }
                FinalOutcome::Done { pending } => pending,
            }
        } else {
            tokio::task::yield_now().await;
            None
        }
    }

    /// The notices of the monitors and links this actor holds, owed since the
    /// watched actors ended, delivered once each through `handle` (none under
    /// a `Kill`, the stop lane re-read before each): after a supervisor's
    /// child reports, since the death plane outranks them. A failed
    /// delivery's reason joins `pending_exit` unless a `Failure` already
    /// stands there.
    async fn owed_notices(
        &mut self,
        drain: &mut ReplyDrain,
        pending_exit: &mut Option<StopReason>,
    ) {
        let ctx = self.ctx_slot.get_mut();
        if !matches!(self.stop_reason, StopReason::Kill)
            && ctx.has_end_state()
            && (ctx.monitor_count() != 0 || ctx.link_work())
        {
            match Box::pin(monitor_drain(
                self.actor_slot.get_mut(),
                ctx,
                held_lane(&mut self.stop_rx),
                &self.stop_reason,
                &self.turn_clock,
                self.mailbox_slot.mailbox_mut(),
                self.priority_slot.lane_mut().as_mut(),
                drain,
                &mut self.held,
            ))
            .await
            {
                FinalOutcome::Killed => self.stop_reason = StopReason::Kill,
                FinalOutcome::Done { pending } => {
                    if !matches!(pending_exit, Some(StopReason::Failure(_))) {
                        *pending_exit = pending.or(pending_exit.take());
                    }
                }
            }
        }
    }

    /// `on_stopped`, in a bracketed turn, and its result. The loop can end
    /// for a reason that read no lane (a handler's `Err` or panic, an `Err`
    /// from a child report, an exhausted budget, a closed channel): a `Kill`
    /// stored by now still skips `on_stopped`, so no hook of the target begins
    /// after the one that was running returns. For a supervisor that
    /// delivered reports this is the read that closes the deliveries. A
    /// failed final delivery (`pending_exit`) makes the exit reason a failure
    /// once `on_stopped` has returned; `on_stopped`'s own error or panic is
    /// applied after it, the same way, and never masks a `Failure` that
    /// stands.
    async fn last_hook(&mut self, pending_exit: Option<StopReason>) {
        if !matches!(self.stop_reason, StopReason::Kill)
            && kill_stored(held_lane(&mut self.stop_rx))
        {
            self.stop_reason = StopReason::Kill;
        }
        if !matches!(self.stop_reason, StopReason::Kill) {
            let actor = self.actor_slot.get_mut();
            let ctx = self.ctx_slot.get_mut();
            let stop_reason = &self.stop_reason;
            let stopped = hook_turn!(ctx, HookKind::OnStopped, guarded, |token| catch_hook_t(
                token,
                || actor.on_stopped(stop_reason, ctx)
            ));
            if let Some(pending) = pending_exit {
                self.stop_reason = pending;
            }
            match stopped {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    // A cleanup error must not mask the original failure.
                    if !matches!(self.stop_reason, StopReason::Failure(_)) {
                        self.stop_reason = StopReason::Failure(err);
                    }
                }
                Err(payload) => {
                    let panic_err = ActorError::Panic(payload_into_string(payload));
                    ctx.record_failure(panic_err.clone());
                    // A cleanup panic must not mask the original failure.
                    if !matches!(self.stop_reason, StopReason::Failure(_)) {
                        self.stop_reason = StopReason::Failure(panic_err);
                    }
                }
            }
        }
    }

    /// The last read before the lane narrows: a `Kill` raised while
    /// `on_stopped` ran, or during a failed init, is the final reason. The
    /// reason `on_stopped` already received is never rewritten, and the read
    /// never consumes the lane's version.
    fn last_kill_read(&mut self) {
        if !matches!(self.stop_reason, StopReason::Kill)
            && kill_stored(held_lane(&mut self.stop_rx))
        {
            self.stop_reason = StopReason::Kill;
        }
    }

    /// Closes the system receiver BEFORE stopping children, and narrows the
    /// stop lane to `Kill`: late system senders (escalation timers, new
    /// get_status callers) fail at once instead of parking, a get_status
    /// already queued fails with the actor's reason once that reason is final
    /// at the seal, and from this exact point
    /// `ActorHandle::stop`/`ActorSystem::stop` answer `SendError::Closed` for
    /// every reason but `Kill`. A `Kill` is still accepted: the teardown below
    /// observes it and turns the rest of itself into the `Kill` path, and the
    /// lane seals only just before the queued messages and the actor value
    /// are dropped. The receiver stays alive until then.
    fn narrow_inputs(&mut self) {
        drop(self.system_rx.take());
        self.ctx_slot.get_mut().stop_lane().close_to_non_kill();
    }

    /// Closes (not drops: the receivers stay alive for the explicit drop
    /// further down) the two user lanes at the same point the inputs narrow:
    /// every sender's `is_closed()` becomes true and every `send`/`try_send`
    /// against it starts returning `Closed`, immediately, from here through
    /// the rest of this actor's teardown (the child cascade included). Without
    /// this, `notify`/`try_notify`/`is_alive` would keep reporting this actor
    /// as reachable for as long as the child cascade takes, even though the
    /// stop lane and system channel already agree it is not, and any message
    /// accepted in that window is simply discarded, never handled, once this
    /// task returns.
    fn close_queues(&mut self) {
        let mailbox = self.mailbox_slot.mailbox_mut();
        mailbox.close();
        if let Some(priority) = self.priority_slot.lane_mut().as_mut() {
            // The lane is tokio's bounded channel, and closing it wakes the
            // senders parked for room, whose wakers belong to their own tasks:
            // a panic of one stays in this call, and the reason is still the
            // one decided. Tokio ends its wake at that waker, so the senders
            // behind it may stay unwoken, and their futures find the lane
            // closed at their next poll.
            let who = self.ctx_slot.get_mut().actor_id().as_who();
            contain_waker_panic("priority lane", Some(who), || priority.close());
        }
    }

    /// Phase 5: stops all children (reverse start order, a pool all at once,
    /// honoring Shutdown), except when this task is itself dying with Kill:
    /// that reason never awaits its own children. Its supervision state, the
    /// death plane included, is dropped directly instead, which aborts every
    /// live child's runner task; each one's link guard fires from its own
    /// drop glue, raising Kill on that child's lane and repeating the same
    /// inversion one level down, without this task ever awaiting the subtree
    /// it is discarding. The first panic of a drop the teardown met is then
    /// applied to the reason.
    async fn stop_children(&mut self, owner: &Owner) {
        let ctx = self.ctx_slot.get_mut();
        // Boxed, and skipped without a supervision state, where it returns
        // `Done` at once: only a supervisor's teardown pays for this state.
        if !matches!(self.stop_reason, StopReason::Kill)
            && ctx.supervision_ref().is_some()
            && Box::pin(stop_all_children(
                ctx,
                held_lane(&mut self.stop_rx),
                &mut self.teardown_panic,
            ))
            .await
                == Teardown::Killed
        {
            self.stop_reason = StopReason::Kill;
        }
        if matches!(self.stop_reason, StopReason::Kill) {
            if let Some(sup) = ctx.take_supervision() {
                keep_first_of(&mut self.teardown_panic, end_supervision(sup, owner), owner);
            }
        }
        if let Some(payload) = self.teardown_panic.take() {
            end_drop_panicked(
                &mut self.stop_reason,
                payload,
                owner,
                ctx.stop_lane().abort_record(),
            );
        }
    }

    /// Frees the name: `RegistryGuard::drop` unregisters the actor, so the
    /// registered name is free from here, before the queued messages, the
    /// value and the context are dropped and before its status plane reads as
    /// ended. The timers and streams are released from the mailbox's close
    /// on, which can come before this point. A caller that wakes on `Stopped`
    /// (or on a closed plane) can reuse the name at once, and tokio completes
    /// this task's JoinHandle only after every drop, so the name is free
    /// before the parent acts on the death either way.
    fn free_name(&mut self, owner: &Owner) {
        drop(self.registry_guard.take());
        #[cfg(test)]
        end_hooks::run(end_hooks::GUARD_RELEASED, owner);
        #[cfg(not(test))]
        let _ = owner;
    }

    /// The one read of the signal maps' panics, then the panic of a
    /// forwarder's drop at the end: every forwarder is ended and joined, so no
    /// map function runs from here on, and the reason is not yet chosen. It
    /// covers every window since the loop ended (the final child reports, the
    /// stage's deliveries, `on_stopped`, the children's stop), whichever
    /// signal's map panicked.
    fn late_panics(&mut self, end_panic: Option<String>, owner: &Owner) {
        let ctx = self.ctx_slot.get_mut();
        collect_map_panic(ctx, &mut self.stop_reason);
        if let Some(payload) = end_panic {
            end_drop_panicked(
                &mut self.stop_reason,
                payload,
                owner,
                ctx.stop_lane().abort_record(),
            );
        }
    }

    /// Seals the lane: every `Kill` a caller was told was accepted was either
    /// observed above or is read here, and every request from here on is
    /// refused with `Closed`, just before the queued messages and the actor
    /// value are dropped. The receiver is dropped with it. The cause a `Kill`
    /// carries comes back with it, read in the same step and returned: every
    /// classification above and below the seal sees `Kill`, and only the
    /// final reason is the cause.
    fn seal(&mut self) -> Option<StopReason> {
        let (sealed_reason, kill_cause) = self.ctx_slot.get_mut().stop_lane().seal_caused();
        if let Some(StopReason::Kill) = sealed_reason {
            self.stop_reason = StopReason::Kill;
        }
        drop(self.stop_rx.take());
        kill_cause
    }

    /// The reason is final here and the name is free. The answer to the
    /// request whose handler failed goes out now, unless a `Kill` ended the
    /// actor: a caller that looks the actor up by name on reading the answer
    /// finds nothing or the next incarnation, never this one. It is sent
    /// before the reason is written, so a caller that finds its queue closed
    /// reads the answer after the reason (`ActorHandle::send`), and a `Kill`
    /// accepted before the seal is the reason the caller reads.
    fn answer_held(&mut self, owner: &Owner) {
        if let Some(payload) = self.held.deliver(&self.stop_reason) {
            end_drop_panicked(
                &mut self.stop_reason,
                payload,
                owner,
                self.ctx_slot.get_mut().stop_lane().abort_record(),
            );
        }
        #[cfg(test)]
        end_hooks::run(end_hooks::ANSWER, owner);
        #[cfg(not(test))]
        let _ = owner;
    }

    /// Writes the final reason into the status plane, once: a panic in a
    /// forwarder's drop at the end already made any reason but a `Kill` or a
    /// `Failure` a failure, and the seal's `Kill` is read. Readers of the
    /// plane (a request whose reply is dropped, `wait_stopped`, a stop report)
    /// read this reason, as the task's return does. A `Kill` raised for an
    /// exit signal ends the actor with that signal's reason, returned as the
    /// cause. When this call wrote the reason, the end event follows the
    /// writer, after the name is free and the lane sealed, with no guard held.
    /// Returns the cause and the identity fields of the log lines that follow.
    fn write_reason(&mut self, kill_cause: Option<StopReason>) -> (Option<StopReason>, EndFields) {
        let record = self.ctx_slot.get_mut().stop_lane().abort_record();
        let cause_reason = final_cause(&self.stop_reason, kill_cause);
        let ended_with = cause_reason.as_ref().unwrap_or(&self.stop_reason);
        let ended = write_final_reason(&self.status, ended_with)
            .then(|| emit_actor_ended(None, record, ended_with))
            .flatten();
        let fields = end_fields(ended, record);
        (cause_reason, fields)
    }

    /// Resolves the init ack, after the seal and before the queued messages
    /// and the value are dropped: however long one of their `Drop`s takes, a
    /// failed init is reported as its error, a `Kill` its caller raises on
    /// reading the error is refused and leaves that error the reason, and a
    /// same-name respawn from the caller never observes NameTaken (OTP 26
    /// corpse-consumed parity). An init a `Kill` cut drops the ack unanswered.
    /// So does a failed init whose end a `Kill` reached before the seal: the
    /// final reason is `Kill`, the awaited spawn reads `Killed` from the
    /// plane, one answer, and the init's error is logged once with the actor's
    /// id.
    fn answer_ack(&mut self, owner: &Owner, fields: &EndFields) {
        #[cfg(not(feature = "tracing"))]
        let _ = fields;
        if let Some(tx) = self.ack.take() {
            match std::mem::replace(&mut self.init_result, Ok(())) {
                Err(err) if !matches!(self.stop_reason, StopReason::Kill) => {
                    let _ = send_contained(tx, Err(err));
                }
                // A failed init that a `Kill` reached before the seal: the ack
                // is dropped unanswered. The drop wakes the spawn's caller,
                // whose waker belongs to the caller's task, and a panic of it
                // stays out of this task, which ends with the reason it has.
                Err(err) => {
                    #[cfg(feature = "tracing")]
                    caught_log!(warn, target: "tokio_actors::actor::runtime",
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
    }

    /// Drops what the actor owned, once its reason is final and its ack
    /// answered: the queued messages one at a time, each drop caught (the
    /// mailbox and the priority lane were closed when the message loop ended),
    /// then the actor value and the context, which holds nothing of the
    /// actor's own by then. The crash report's text is made from the value
    /// before it drops and returned. A panic in any of these drops is caught
    /// and changes nothing: the reason is final, the end goes on, `Stopped` is
    /// still published, and with the `tracing` feature each panic is logged
    /// once with the actor's id: the first below, a later one where it is
    /// discarded. `cause` is the cause a `Kill` carried, if any.
    fn drop_owned(
        &mut self,
        cause: Option<&StopReason>,
        owner: &Owner,
        fields: &EndFields,
    ) -> Option<reports::CrashText> {
        let ctx = self.ctx_slot.get_mut();
        let record = ctx.stop_lane().abort_record();
        let mut late_panic = self.mailbox_slot.end(Some(record));
        keep_first_of(&mut late_panic, self.priority_slot.end(Some(record)), owner);
        let crash = reports::crash_text(
            self.actor_slot.peek(),
            &self.stop_reason,
            record,
            ctx.debug_ring(),
            &self.turn_clock,
        );
        keep_first_of(&mut late_panic, self.actor_slot.end(), owner);
        keep_first_of(&mut late_panic, self.ctx_slot.end(), owner);
        #[cfg(feature = "tracing")]
        if let Some(payload) = &late_panic {
            let ended_with = cause.unwrap_or(&self.stop_reason);
            caught_log!(warn, target: "tokio_actors::actor::runtime",
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
        let _ = (late_panic, cause, fields);
        crash
    }

    /// Writes the crash report and the line of the actor's end, once
    /// `Stopped` is published. `cause` is the cause a `Kill` carried, if any.
    fn report_end(
        &self,
        crash: Option<reports::CrashText>,
        cause: Option<&StopReason>,
        owner: &Owner,
        fields: &EndFields,
    ) {
        #[cfg(feature = "tracing")]
        {
            reports::crash_log(crash, owner, fields, &self.stop_reason);
            caught_log!(info, target: "tokio_actors::actor::runtime",
                actor_id = %owner,
                actor_name = fields.name(),
                incarnation = fields.incarnation(),
                system = fields.system(),
                reason = %fields.reason_text(cause.unwrap_or(&self.stop_reason)),
                "Actor stopped"
            );
        }
        #[cfg(not(feature = "tracing"))]
        {
            reports::crash_log(crash, owner);
            let _ = (cause, fields);
        }
    }
}

/// The identity fields of the end's log lines, from the identity the end
/// event read.
#[cfg(feature = "tracing")]
fn end_fields(ended: Option<Arc<Identity>>, record: &IncarnationRef) -> EndFields {
    crate::telemetry::LogFields::released(ended, record)
}

/// Without the `tracing` feature the identity is let go at once.
#[cfg(not(feature = "tracing"))]
fn end_fields(ended: Option<Arc<Identity>>, _: &IncarnationRef) -> EndFields {
    drop(ended);
    EndFields
}

/// Drops a supervisor's state: its child factories one at a time, each drop
/// caught, then the rest.
pub(super) fn end_supervision(mut sup: SupervisionState, owner: &Owner) -> Option<String> {
    let mut first = None;
    for (_, factory) in std::mem::take(&mut sup.restart_fns) {
        keep_first_of(&mut first, end_value(factory), owner);
    }
    keep_first_of(&mut first, end_value(sup), owner);
    first
}

/// Drops an actor's context: its supervision state first, as
/// [`end_supervision`] does, then the rest, each drop caught.
pub(super) fn end_context<A: Actor>(mut ctx: ActorContext<A>, owner: &Owner) -> Option<String> {
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
pub(super) fn registry_parent(
    is_root: bool,
    parent: Option<(&ActorId, u64)>,
) -> Option<(ActorId, u64)> {
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
    record: &IncarnationRef,
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
    record: &IncarnationRef,
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
    record: &IncarnationRef,
) {
    if !matches!(stop_reason, StopReason::Kill | StopReason::Failure(_)) {
        *stop_reason = StopReason::Failure(ActorError::Panic(payload));
        return;
    }
    #[cfg(feature = "tracing")]
    {
        let fields = crate::telemetry::log_fields(record, None);
        caught_log!(warn, target: "tokio_actors::actor::runtime",
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
