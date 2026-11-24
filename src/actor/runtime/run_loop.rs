//! The actor's task: the start hooks and the message loop with its biased branches.

use std::borrow::Cow;
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use super::{
    advance_stop_queue, answer_status, await_strict_children, chain_after, check_verdicts,
    continue_chain, defer_armed, deliver, dispatch_one, finish_turn, fire_idle, flow_of,
    fp_skip_step, hand_back, hand_back_idle, handle_child_stopped, handle_restart_outcome,
    kill_stored, lane_changed, mail_or_idle, next_supervision_event, open_signal, poll_now,
    recv_optional, requeue, settle, turn_done_hot, undelivered_panic, upper_idle, AbortReason,
    CaughtDrop, End, HeldAnswer, LoopEnd, QueueSlot, ReplyDrain, SlotHold, StrictOutcome,
    SupervisionEvent, TurnEnd, Wake, TURN_QUANTUM,
};
use crate::actor::context::ActorContext;
use crate::actor::envelope::{Envelope, SystemMessage};
use crate::actor::incarnation::turn_clock::{
    now_nanos, outcome_of, outcome_plain, push_turn, TurnGuard,
};
use crate::actor::incarnation::{LaneRx, StatusTx, StopSeverity, SubtreeHold, SystemPlane};
use crate::actor::mailbox::MailRx;
use crate::actor::panic::{catch_hook_t, discard, payload_into_string, send_contained, AnswerHold};
#[cfg(test)]
use crate::actor::runtime::{end_hooks, fast_path_turns, hand_backs, loop_facts};
use crate::actor::supervision::rules::{init_kill_check, InitStep};
use crate::actor::sys_loop::is_suspended;
#[cfg(feature = "sys")]
use crate::actor::sys_serve;
use crate::actor::{sys_state, Actor, ActorEnvelope};
use crate::error::ActorError;
use crate::system::RegistryGuard;
use crate::telemetry::{
    armed_for, push_about, EventKind, HookKind, HotCache, TelemetryEvent, TurnOutcome,
};
use crate::types::{ActorStatus, ChildStoppedInternal, StopReason};

// ---------------------------------------------------------------------------
// Actor run loop
// ---------------------------------------------------------------------------

/// How a hook's caught result ended, for its turn event: a hook that returns
/// a `Result` reads as `outcome_of` says, `pre_stop`'s `bool` as
/// `outcome_plain` says.
pub(super) trait HookOutcome {
    fn outcome(&self) -> TurnOutcome;
}

impl<T, E> HookOutcome for Result<Result<T, E>, crate::actor::panic::PanicPayload> {
    fn outcome(&self) -> TurnOutcome {
        outcome_of(self)
    }
}

impl HookOutcome for Result<bool, crate::actor::panic::PanicPayload> {
    fn outcome(&self) -> TurnOutcome {
        outcome_plain(self)
    }
}

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
    pub(super) _subtree: Arc<SubtreeHold>,
    // A sender of the status plane, declared second: the tail publishes
    // `Stopped` through it once everything the actor owned is dropped. On
    // return and on an abort while suspended it is dropped after every
    // parameter declared below it, so the plane stays open until the actor
    // value, the context and the queued messages are gone. On an abort
    // before the first poll the registry guard holds the plane open
    // the same way it holds the subtree. Each timer and stream forwarder
    // holds both signals too, so the plane and the subtree also outlive the
    // forwarders' futures.
    pub(super) status: StatusTx,
    // Everything the actor owns, each in a `CaughtDrop`: dropped any way
    // other than by the tail below (an abort, before or after the first
    // poll, or the task's own unwind), its drop still catches every panic,
    // so drop glue never unwinds and the task ends as the abort or the
    // unwind does.
    pub(super) actor_slot: CaughtDrop<A>,
    pub(super) ctx_slot: CaughtDrop<ActorContext<A>>,
    pub(super) mailbox_slot: QueueSlot<MailRx<ActorEnvelope<A>>>,
    pub(super) priority_slot: QueueSlot<mpsc::Receiver<ActorEnvelope<A>>>,
    // Declared right after the queues, never a local of the body: on an abort
    // while suspended, or this task's own unwind, the parameters drop last to
    // first, so it writes the abort reason after `ack` and `registry_guard`
    // (the name is free) and before the queued messages, the context and the
    // value drop. A local would drop before `registry_guard`, and a reader of
    // the reason could meet the name still registered. On return the seal
    // wrote the reason first and it writes nothing.
    pub(super) _abort_reason: AbortReason,
    pub(super) system_rx: SystemPlane,
    pub(super) stop_rx: LaneRx,
    // Clears the turn in flight when the task ends on any path.
    pub(super) turn_clock: TurnGuard,
    // Declared after everything the actor owns: on an abort before the first
    // poll it frees the name, then releases its status sender and its
    // subtree hold, after the value, the context and the queued messages
    // are dropped.
    pub(super) registry_guard: Option<RegistryGuard>,
    // The start ack, in a guard: the task's own unwind drops it with the other
    // parameters, and the caller's waker it wakes then must not add a second
    // panic to the unwind.
    pub(super) ack: AnswerHold<Result<(), ActorError>>,
    // What the run loop needs to know about the runtime its task runs on, read
    // once at the spawn from the handle the task is spawned on (the placement
    // runtime when the spawn names one): how many worker threads it has, and
    // whether it is a current-thread runtime. The two are separate facts: a
    // multi-thread runtime with one worker has one worker and is not a
    // current-thread runtime. Declared last and plain values: they own
    // nothing, so no drop order depends on them.
    pub(super) workers: usize,
    pub(super) same_thread: bool,
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

pub(super) async fn run_actor<A: Actor>(parts: Box<RunParts<A>>) -> StopReason {
    // The end takes what the loop left by value: the run future holds the
    // loop's future or the end's, never both.
    run_loop(parts).await.end().await
}

/// The start and the message loop of the actor's task. Returns what the end
/// works on, every parameter and every local the end still needs moved into
/// it: the bindings of this body end with it, so the run future keeps each
/// value once, and the end's fields drop in the order these bindings did.
async fn run_loop<A: Actor>(parts: Box<RunParts<A>>) -> End<A> {
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
            let phase1 = hook_turn!(ctx, HookKind::PreStart, guarded, |token| catch_hook_t(
                token,
                || actor.pre_start(ctx)
            ));
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
            let phase2 = hook_turn!(ctx, HookKind::OnStarted, guarded, |token| catch_hook_t(
                token,
                || actor.on_started(ctx)
            ));
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
    let teardown_panic: Option<String> = None;
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
    let looped = if started {
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
                        let pre_stop_result =
                            hook_turn!(ctx, HookKind::PreStop, guarded, |token| catch_hook_t(token, || actor.pre_stop(&reason, ctx)));
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
                            #[cfg(feature = "sys")]
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

        Some(LoopEnd {
            hot,
            lane_slot,
            run: (!run_fresh).then_some((takes, turn_start)),
            drain,
        })
    } else {
        None
    };

    End {
        looped,
        stop_reason,
        held,
        teardown_panic,
        init_result,
        ack,
        registry_guard,
        turn_clock,
        stop_rx: Some(stop_rx),
        system_rx: Some(system_rx),
        _abort_reason,
        priority_slot,
        mailbox_slot,
        ctx_slot,
        actor_slot,
        status,
        _subtree,
    }
}
