//! State access: closures that read or change an actor's value. They ride
//! the system channel the actor already has, and the block's bits decide
//! whether the actor allows them, so an actor that does not use them holds
//! no channel, no field and no task for them.
//!
//! A closure is user code. It runs only on the actor's own task, between two
//! turns, as a turn of its own that the watchdog sees, inside a catch; the
//! reply goes out through a guard that releases the caller's sender inside a
//! catch, and a request that is not run is dropped inside a catch.

use super::context::ActorContext;
use super::incarnation::{IncarnationRef, SYS_SENSITIVE};
use super::panic::{catch_sync, payload_into_string};
use super::runtime::build_status_info;
use super::sys_loop::is_suspended;
use super::Actor;
use crate::actor::incarnation::turn_clock::{outcome_plain, push_turn};
use crate::actor::incarnation::TurnClock;
use crate::telemetry::HookKind;
use crate::types::ActorStatusInfo;

/// What `get_status` shows as the state of an actor whose `format_status`
/// panicked.
const FORMAT_STATUS_PANICKED: &str = "format_status panicked";

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
    record: &IncarnationRef,
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
// The parts of this module, in the order the generated docs list their methods.

#[cfg(feature = "sys")]
mod ops;
#[cfg(feature = "sys")]
pub(crate) use self::ops::*;

#[cfg(test)]
#[cfg(feature = "sys")]
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
#[cfg(feature = "sys")]
mod tests {
    use std::ops::ControlFlow;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::sync::oneshot;

    use super::{run, InspectOp, ReplaceOp, StateOp};
    use crate::actor::context::ActorContext;
    use crate::actor::handle::ActorHandle;
    use crate::actor::incarnation::StatusValue;
    use crate::actor::incarnation::SubtreeHold;
    use crate::actor::incarnation::{StatusTx, StopLane, TurnClock};
    use crate::actor::sys::SysError;
    use crate::actor::Actor;
    use crate::error::ActorResult;
    use crate::types::{ActorId, ActorStatus};

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
