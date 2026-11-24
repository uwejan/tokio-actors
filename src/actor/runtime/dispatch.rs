//! One turn: taking a message or a signal, running the handler, the continuation and the idle hook.

use std::borrow::Cow;
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::Poll;

use tokio::task::{JoinError, JoinSet};
use tokio::time::Duration;

use super::{end_value, watch_yield, HeldAnswer, HookOutcome, ReplyDrain};
use crate::actor::context::{ActorContext, ForwarderEnd};
use crate::actor::envelope::Envelope;
use crate::actor::incarnation::turn_clock::{push_turn, TurnMode};
use crate::actor::incarnation::{IncarnationRef, TurnClock};
use crate::actor::mailbox::MailRx;
use crate::actor::panic::{catch_hook_t, payload_into_string, send_contained, AnswerHold};
#[cfg(test)]
use crate::actor::runtime::end_hooks;
use crate::actor::supervision::{DeathOutcome, RestartOutcome};
use crate::actor::sys::DebugEntry;
use crate::actor::turn::IdleKind;
use crate::actor::{Actor, ActorEnvelope};
use crate::error::ActorError;
use crate::telemetry::{push_about, EventKind, HookKind, HotCache, TelemetryEvent, TurnOutcome};
use crate::types::{ActorId, ActorStatus, StopReason};

/// One event from either of the supervisor's two `JoinSet` planes, as
/// returned by [`next_supervision_event`].
pub(super) enum SupervisionEvent {
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
pub(super) async fn next_supervision_event<A: Actor>(
    ctx: &mut ActorContext<A>,
) -> Option<SupervisionEvent> {
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
pub(super) async fn dispatch<A: Actor>(
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
pub(super) async fn continue_chain<A: Actor>(
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
pub(super) async fn fire_idle<A: Actor>(
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
            Some(Err(panic)) => fail_with(ctx, ActorError::Panic(panic)),
        },
        IdleKind::Idle => {
            // A flag left by an earlier `stop` is not this hook's.
            let _ = ctx.take_self_stop();
            let outcome = hook_turn!(ctx, HookKind::OnIdle, guarded, |token| catch_hook_t(
                token,
                || actor.on_idle(ctx)
            ));
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
                    fail_with(ctx, err)
                }
                Err(payload) => {
                    ctx.drop_continuation();
                    fail_with(ctx, ActorError::Panic(payload_into_string(payload)))
                }
            }
        }
    };
    ctx.set_inline(false);
    flow
}

/// What ended a wait on the mailbox and the idle sleep.
pub(super) enum Wake<T> {
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
pub(super) fn mail_or_idle<'a, T>(
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
pub(super) enum TurnEnd {
    /// The loop goes on.
    Next,
    /// The turn ended the actor; `stop_reason` holds why.
    Stop,
}

/// Turns the outcome of a cold path into a [`TurnEnd`], storing the reason of
/// a stop in `stop_reason`.
#[cold]
#[inline(never)]
pub(super) fn settle(flow: ControlFlow<StopReason>, stop_reason: &mut StopReason) -> TurnEnd {
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
pub(super) fn open_signal<A: Actor>(
    ctx: &mut ActorContext<A>,
    tag: crate::actor::envelope::SignalTag,
    payload: A::Message,
) -> Result<A::Message, ControlFlow<StopReason>> {
    if tag.shared.is_flushed() {
        return Err(match end_value(payload) {
            None => ControlFlow::Continue(()),
            Some(panic) => fail_with(ctx, ActorError::Panic(panic)),
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
pub(super) fn deliver<A: Actor>(
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

/// Records `err` as the actor's last failure and ends the actor with it: the
/// exit of a turn, a hook or a delivery that failed, by an `Err` or by a panic
/// the caller made an `ActorError::Panic`.
pub(super) fn fail_with<A: Actor>(
    ctx: &mut ActorContext<A>,
    err: ActorError,
) -> ControlFlow<StopReason> {
    ctx.record_failure(err.clone());
    ControlFlow::Break(StopReason::Failure(err))
}

/// The turn failed because the drop of an undelivered response panicked.
#[cold]
#[inline(never)]
pub(super) fn undelivered_panic<A: Actor>(
    ctx: &mut ActorContext<A>,
    payload: String,
) -> ControlFlow<StopReason> {
    ctx.drop_continuation();
    fail_with(ctx, ActorError::Panic(payload))
}

/// The events of a `handle` turn that returned `Ok` and whose reply was
/// sent, raised on the run loop's own task: the debug state's trace entries
/// of the turn and `TurnFinished`. The loop owes only its next take here, so
/// the handlers run on this task from the loop's cached list. Kept out of the
/// turn: an actor with no debug option and no sink of `TurnFinished` never
/// calls it.
#[cold]
#[inline(never)]
pub(super) fn turn_done_hot<A: Actor>(
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
pub(super) fn finish_turn<A: Actor>(
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
            fail_with(ctx, err)
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
            fail_with(ctx, err)
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
pub(super) async fn dispatch_one<A: Actor>(
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
pub(super) fn flow_of(end: TurnEnd, stop_reason: &mut StopReason) -> ControlFlow<StopReason> {
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
pub(super) async fn chain_after<A: Actor>(
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
pub(super) fn log_discarded_answer(
    panic: Option<String>,
    owner: &ActorId,
    record: &IncarnationRef,
) {
    #[cfg(feature = "tracing")]
    if let Some(panic) = &panic {
        caught_log!(warn, target: "tokio_actors::actor::runtime",
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
