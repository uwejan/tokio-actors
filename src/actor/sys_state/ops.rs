//! State access: the operations `inspect` and `replace_state` send to an actor, and how the actor runs them.

use std::any::Any;
use std::marker::PhantomData;
use std::ops::ControlFlow;

use tokio::sync::oneshot;

use crate::actor::context::ActorContext;
use crate::actor::incarnation::turn_clock::push_turn;
use crate::actor::incarnation::{IncarnationRef, TurnClock, SYS_ALLOW_STATE, SYS_SENSITIVE};
use crate::actor::panic::{
    catch_sync, drop_contained, payload_into_string, send_contained, AnswerHold,
};
use crate::actor::sys::SysError;
#[cfg(test)]
use crate::actor::sys_state::ran;
use crate::actor::Actor;
use crate::error::ActorError;
use crate::telemetry::HookKind;
use crate::types::StopReason;

/// What a reply carries when a request meets an actor of another type than
/// the handle names.
const WRONG_ACTOR_TYPE: &str = "the request names another actor type";

/// One request for the actor's value. The channel it rides carries every
/// actor type, so the request meets the value as `dyn Any` and each
/// implementation takes it back as its own actor type.
pub(crate) trait StateOp: Send {
    /// Whether the caller has gone, so the request need not run.
    fn caller_gone(&self) -> bool;

    /// Runs the request on the actor's value.
    fn run(self: Box<Self>, actor: &mut dyn Any) -> StateDone;
}

/// How a request on the state plane ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StateDone {
    /// The closure returned.
    Done,
    /// A closure that only reads panicked, with this message.
    InspectPanicked(String),
    /// A closure that changes the value panicked, with this message.
    ReplacePanicked(String),
}

/// A request as the system channel carries it.
pub(crate) type StateRequest = Box<dyn StateOp>;

/// Whether the actor of `record` allows its value to be reached: not when
/// it is sensitive, and only when it was spawned with state access.
pub(crate) fn state_access(record: &IncarnationRef) -> Result<(), SysError> {
    let bits = record.sys_bits();
    if bits & SYS_SENSITIVE != 0 {
        Err(SysError::Sensitive)
    } else if bits & SYS_ALLOW_STATE == 0 {
        Err(SysError::NotAllowed)
    } else {
        Ok(())
    }
}

/// Sends `value` to the caller through the guard, inside the catches the
/// send needs, and drops a value nobody takes inside a catch: it is the
/// caller's type, and its drop is user code.
fn answer<T>(reply: &mut AnswerHold<T>, value: T) {
    if let Some(tx) = reply.take() {
        if let Err(unsent) = send_contained(tx, value) {
            drop_contained(unsent);
        }
    }
}

/// A read of the actor's value.
pub(crate) struct InspectOp<A, R, F> {
    f: F,
    reply: AnswerHold<Result<R, SysError>>,
    _actor: PhantomData<fn(&A)>,
}

impl<A, R, F> InspectOp<A, R, F> {
    pub(crate) fn new(f: F, reply: oneshot::Sender<Result<R, SysError>>) -> Self {
        Self {
            f,
            reply: AnswerHold::new(Some(reply)),
            _actor: PhantomData,
        }
    }
}

impl<A, R, F> StateOp for InspectOp<A, R, F>
where
    A: Actor,
    R: Send + 'static,
    F: FnOnce(&A) -> R + Send + 'static,
{
    fn caller_gone(&self) -> bool {
        self.reply.is_closed()
    }

    fn run(self: Box<Self>, actor: &mut dyn Any) -> StateDone {
        let Self { f, mut reply, .. } = *self;
        let Some(actor) = actor.downcast_mut::<A>() else {
            drop_contained(f);
            answer(
                &mut reply,
                Err(SysError::Callback(WRONG_ACTOR_TYPE.to_string())),
            );
            return StateDone::Done;
        };
        match catch_sync(move || f(&*actor)) {
            Ok(value) => {
                answer(&mut reply, Ok(value));
                StateDone::Done
            }
            Err(payload) => {
                let text = payload_into_string(payload);
                answer(&mut reply, Err(SysError::Callback(text.clone())));
                StateDone::InspectPanicked(text)
            }
        }
    }
}

/// A change of the actor's value.
pub(crate) struct ReplaceOp<A, F> {
    f: F,
    reply: AnswerHold<Result<(), SysError>>,
    _actor: PhantomData<fn(&mut A)>,
}

impl<A, F> ReplaceOp<A, F> {
    pub(crate) fn new(f: F, reply: oneshot::Sender<Result<(), SysError>>) -> Self {
        Self {
            f,
            reply: AnswerHold::new(Some(reply)),
            _actor: PhantomData,
        }
    }
}

impl<A, F> StateOp for ReplaceOp<A, F>
where
    A: Actor,
    F: FnOnce(&mut A) + Send + 'static,
{
    fn caller_gone(&self) -> bool {
        self.reply.is_closed()
    }

    fn run(self: Box<Self>, actor: &mut dyn Any) -> StateDone {
        let Self { f, mut reply, .. } = *self;
        let Some(actor) = actor.downcast_mut::<A>() else {
            drop_contained(f);
            answer(
                &mut reply,
                Err(SysError::Callback(WRONG_ACTOR_TYPE.to_string())),
            );
            return StateDone::Done;
        };
        match catch_sync(move || f(actor)) {
            Ok(()) => {
                answer(&mut reply, Ok(()));
                StateDone::Done
            }
            Err(payload) => {
                let text = payload_into_string(payload);
                answer(&mut reply, Err(SysError::Callback(text.clone())));
                StateDone::ReplacePanicked(text)
            }
        }
    }
}

/// Serves one state request on the actor's task, between two turns.
///
/// A request whose caller has gone is dropped unrun, inside a catch. Any
/// other runs as a turn of its own, bracketed for the watchdog, inside a
/// catch, and runs to its end even if its caller's bound passes meanwhile. A change that panicked may have left the value torn, so the actor
/// ends with that panic as its failure.
pub(crate) fn run<A: Actor>(
    op: StateRequest,
    actor: &mut A,
    ctx: &mut ActorContext<A>,
    clock: &TurnClock,
) -> ControlFlow<StopReason> {
    if op.caller_gone() {
        drop_contained(op);
        return ControlFlow::Continue(());
    }
    #[cfg(test)]
    ran::bump();
    let turn = clock.begin();
    let done = op.run(actor);
    let took = clock.end_timed(turn);
    let outcome = match done {
        StateDone::Done => crate::telemetry::TurnOutcome::Returned,
        StateDone::InspectPanicked(_) | StateDone::ReplacePanicked(_) => {
            crate::telemetry::TurnOutcome::Panicked
        }
    };
    push_turn(
        ctx.stop_lane().abort_record(),
        turn,
        took,
        HookKind::Sys,
        outcome,
    );
    match done {
        StateDone::ReplacePanicked(text) => {
            let error = ActorError::Panic(text);
            ctx.record_failure(error.clone());
            ControlFlow::Break(StopReason::Failure(error))
        }
        StateDone::Done | StateDone::InspectPanicked(_) => ControlFlow::Continue(()),
    }
}

/// How many state requests the calling thread has served, for the tests of
/// state access.
#[cfg(test)]
pub(crate) fn runs_this_thread() -> usize {
    ran::count()
}
