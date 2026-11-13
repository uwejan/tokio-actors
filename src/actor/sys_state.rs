//! State access: closures that read or change an actor's value. They ride
//! the system channel the actor already has, and the block's bits decide
//! whether the actor allows them, so an actor that does not use them holds
//! no channel, no field and no task for them.
//!
//! A closure is user code. It runs only on the actor's own task, between two
//! turns, as a turn of its own that the watchdog sees, inside a catch; the
//! reply goes out through a guard that releases the caller's sender inside a
//! catch, and a request that is not run is dropped inside a catch.

use std::any::Any;
use std::marker::PhantomData;
use std::ops::ControlFlow;

use tokio::sync::oneshot;

use super::context::ActorContext;
use super::panic::{catch_sync, payload_into_string, send_contained, AnswerHold};
use super::runtime::build_status_info;
use super::subtree::{AbortRecord, SYS_ALLOW_STATE, SYS_SENSITIVE};
use super::sys::SysError;
use super::sys_loop::is_suspended;
use super::Actor;
use crate::error::ActorError;
use crate::telemetry::handlers::drop_contained;
use crate::telemetry::HookKind;
use crate::types::turn_clock::{outcome_plain, push_turn};
use crate::types::{ActorStatusInfo, StopReason, TurnClock};

/// What `get_status` shows as the state of an actor whose `format_status`
/// panicked.
const FORMAT_STATUS_PANICKED: &str = "format_status panicked";

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
pub(crate) fn state_access(record: &AbortRecord) -> Result<(), SysError> {
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

/// The status a `get_status` call is answered with: the counts of
/// `build_status_info`, the suspension and the actor's own account of its
/// state. The account is asked first and the answer is built once, so the
/// counts are as recent as the answer and no half-built value is copied.
pub(crate) fn status_info<A: Actor>(
    actor: &A,
    ctx: &ActorContext<A>,
    clock: &TurnClock,
) -> ActorStatusInfo {
    let record = ctx.stop_lane().abort_record();
    let suspended = is_suspended(ctx);
    let state = format_status_safely(actor, record.sys_bits() & SYS_SENSITIVE != 0, record, clock);
    build_status_info(ctx, suspended, state)
}

/// The actor's `format_status`, for a status reply or a crash report: never
/// called for a sensitive actor, run as a turn of its own that the watchdog
/// sees, inside a catch. A panic gives a placeholder and changes nothing
/// else.
pub(crate) fn format_status_safely<A: Actor>(
    actor: &A,
    sensitive: bool,
    record: &AbortRecord,
    clock: &TurnClock,
) -> Option<String> {
    if sensitive {
        return None;
    }
    let turn = clock.begin();
    let formatted = catch_sync(|| actor.format_status());
    let took = clock.end_timed(turn);
    push_turn(record, turn, took, HookKind::Sys, outcome_plain(&formatted));
    match formatted {
        Ok(text) => text,
        Err(payload) => {
            drop(payload_into_string(payload));
            Some(FORMAT_STATUS_PANICKED.to_string())
        }
    }
}

/// How many state requests the calling thread has served, for the tests of
/// state access.
#[cfg(test)]
pub(crate) fn runs_this_thread() -> usize {
    ran::count()
}

#[cfg(test)]
mod ran {
    use std::cell::Cell;

    thread_local! {
        static RUNS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn bump() {
        RUNS.with(|runs| runs.set(runs.get() + 1));
    }

    pub(super) fn count() -> usize {
        RUNS.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::sync::oneshot;

    use super::{run, InspectOp, ReplaceOp, StateOp};
    use crate::actor::context::ActorContext;
    use crate::actor::handle::ActorHandle;
    use crate::actor::subtree::SubtreeHold;
    use crate::actor::sys::SysError;
    use crate::actor::Actor;
    use crate::error::ActorResult;
    use crate::types::{ActorId, ActorStatus, StatusTx, StatusValue, StopLane, TurnClock};

    struct Holder;

    impl Actor for Holder {
        type Message = ();
        type Response = ();

        async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
    }

    /// Counts its drop, then panics.
    struct PanicsOnDrop(Arc<AtomicUsize>);

    impl Drop for PanicsOnDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("the captured value panicked when it was dropped");
        }
    }

    /// One row: the kind of request and how it is built from a drop counter.
    type Row = (&'static str, fn(Arc<AtomicUsize>) -> Box<dyn StateOp>);

    fn context() -> ActorContext<Holder> {
        let (tx, _rx) = crate::actor::mailbox::test_channel(1);
        let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, _) = StopLane::new();
        let id = ActorId::from("state-entry");
        let handle = ActorHandle::new(
            id.clone(),
            tx,
            stop_lane,
            1,
            status_rx,
            None,
            Some(Arc::new(AtomicBool::new(false))),
        );
        ActorContext::new(
            id,
            handle,
            tokio::runtime::Handle::current(),
            status_tx,
            None,
            None,
            None,
            TurnClock::default(),
            SubtreeHold::new(None),
        )
    }

    /// A request whose caller has already left, meeting the entry directly,
    /// is dropped unrun with a panic of its captured value contained: the
    /// entry returns and asks for nothing more. Each row is one kind of
    /// request.
    async fn a_request_whose_caller_left_is_dropped_inside_a_catch() {
        let rows: [Row; 2] = [
            ("inspect", |drops| {
                let guard = PanicsOnDrop(drops);
                let (tx, rx) = oneshot::channel::<Result<(), SysError>>();
                drop(rx);
                Box::new(InspectOp::<Holder, (), _>::new(
                    move |_: &Holder| {
                        let _guard = &guard;
                    },
                    tx,
                ))
            }),
            ("replace", |drops| {
                let guard = PanicsOnDrop(drops);
                let (tx, rx) = oneshot::channel::<Result<(), SysError>>();
                drop(rx);
                Box::new(ReplaceOp::<Holder, _>::new(
                    move |_: &mut Holder| {
                        let _guard = &guard;
                    },
                    tx,
                ))
            }),
        ];
        for (name, request) in rows {
            let drops = Arc::new(AtomicUsize::new(0));
            let mut ctx = context();
            let flow = run(
                request(drops.clone()),
                &mut Holder,
                &mut ctx,
                &TurnClock::default(),
            );
            assert!(
                matches!(flow, ControlFlow::Continue(())),
                "{name}: the actor goes on"
            );
            assert_eq!(
                drops.load(Ordering::SeqCst),
                1,
                "{name}: the capture was dropped"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_whose_caller_left_is_dropped_inside_a_catch_multi_thread() {
        a_request_whose_caller_left_is_dropped_inside_a_catch().await;
    }
}
