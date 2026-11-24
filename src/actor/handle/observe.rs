//! Reading an actor from outside: liveness, status, `wait_stopped` and the subtree observer.

use tokio::sync::oneshot;

use super::{ActorHandle, SubtreeObserver};
use crate::actor::incarnation::{StatusRx, SubtreeWait, SubtreeWatch};
use crate::actor::supervision::rules::{successor_wait_step, WaitAction};
use crate::actor::Actor;
use crate::error::AskError;
#[cfg(doc)]
use crate::system::ActorSystem;
use crate::system::{wait_closed, wait_reason};
#[cfg(doc)]
use crate::types::ActorStatus;
use crate::types::{ActorStatusInfo, StopReason, SubtreeEnd};

impl<A: Actor> ActorHandle<A> {
    /// Requests a status snapshot from the actor.
    ///
    /// This bypasses the mailbox and uses the system channel, so it
    /// responds even when the mailbox is full.
    ///
    /// # Calling oneself
    /// A call from the actor's own task, such as a callback on
    /// `ctx.self_handle()`, returns [`AskError::CallingSelf`] at once, as
    /// [`send`](Self::send) does: the actor cannot service its system channel
    /// while it is busy running that very callback. A task spawned from a
    /// callback that calls this actor while the callback awaits it still parks
    /// forever. A descendant that queries this
    /// actor while this actor is stopping has the same shape: the query is
    /// answered with the actor's end, and the actor's teardown waits for the
    /// descendant, so under [`Shutdown::Infinity`](crate::types::Shutdown::Infinity)
    /// neither returns. For a hung actor's status without these risks, use
    /// the always-answering [`status`](Self::status).
    ///
    /// # Errors
    /// - [`AskError::CallingSelf`] if the call is made from the actor's own
    ///   task (nothing was sent).
    /// - [`AskError::Closed`] if the actor's system channel is closed: the
    ///   actor has ended, or is past its message loop.
    /// - [`AskError::ResponseDropped`] if the actor stopped after accepting
    ///   the query but before answering. It carries the reason the actor
    ///   ended with, and is returned once that reason is final, which waits
    ///   through the rest of the actor's teardown (the stop of its children
    ///   included) and has no bound of its own: bound the call with
    ///   `tokio::time::timeout` when the target's teardown may not return.
    pub async fn get_status(&self) -> Result<ActorStatusInfo, AskError> {
        self.refuse_self_call()?;
        let (tx, rx) = oneshot::channel();
        self.inner
            .stop_lane
            .abort_record()
            .submit_status(tx)
            .await
            .map_err(|()| AskError::Closed)?;
        match rx.await {
            Ok(info) => Ok(*info),
            Err(_) => Err(self.response_dropped().await),
        }
    }

    /// Waits until the actor reaches its terminal [`ActorStatus::Stopped`]
    /// state and returns the reason it stopped with.
    ///
    /// Called on oneself from one of the actor's own callbacks it never
    /// completes: the actor cannot end while the callback waits for its end.
    /// It is not refused, since a waiter on another task is legitimate.
    ///
    /// Resolves for every way an actor can die: a graceful stop, a kill, and
    /// even a task abort. An abort drops the status sender without a final
    /// status write (nobody is left to report `Stopped`), so an actor-side
    /// sender drop is also treated as terminal here: the wait still returns
    /// instead of hanging forever, with [`StopReason::Kill`], the reason of an
    /// aborted actor. For an actor that is already stopped the call returns
    /// at once.
    ///
    /// The reason is the one the actor's supervisor receives when it has one:
    /// every party waiting on the actor's end reads one terminal reason. A
    /// start cut short by its own `start_timeout` reads `Kill` here, since
    /// the cut that ends it is a kill.
    ///
    /// Uses the same runtime status plane as [`status`](Self::status), so it
    /// resolves even for an actor hung inside a callback (for any caller but
    /// that actor's own callback, see above).
    ///
    /// This resolves when THIS actor has stopped. A supervising parent
    /// observes the same death separately, in its own run loop, so
    /// [`on_child_stopped`](crate::actor::Actor::on_child_stopped) may run
    /// slightly after this call returns.
    ///
    /// # Wakers
    /// A waker belongs to the task that polls with it, and the crate wakes it
    /// on that task's behalf when the actor's status changes. A panic of that
    /// waker is caught where the crate wakes it: it never reaches the actor,
    /// which ends with the reason it has. Tokio ends its wake of a list of
    /// waiting tasks at the waker that panics, so the tasks behind it in that
    /// list may be left unwoken, and the waits on one actor's status share one
    /// list: this call, the first part of
    /// [`SubtreeObserver::wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended)
    /// and the crate's own waits on the actor. A call behind a waker that
    /// panics may therefore stay pending after the actor has stopped, and
    /// returns when its task is next polled, not before. Poll it from a task
    /// whose waker does not panic.
    pub async fn wait_stopped(&self) -> StopReason {
        let mut rx = self.inner.status_rx.clone();
        wait_reason(&mut rx).await
    }

    /// Returns an observer of the subtree of this incarnation: the actor and
    /// every actor below it. It works before and after the actor's end, so it
    /// needs no ordering with a stop or a kill. It names the incarnation this
    /// handle names; a handle kept across a supervised restart names the
    /// earlier one.
    ///
    /// Await it with
    /// [`SubtreeObserver::wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended).
    pub fn observe_subtree(&self) -> SubtreeObserver {
        SubtreeObserver::new(self.inner.status_rx.clone())
    }
}

impl std::fmt::Debug for SubtreeObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubtreeObserver").finish_non_exhaustive()
    }
}

impl SubtreeObserver {
    pub(crate) fn new(status_rx: StatusRx) -> Self {
        Self { status_rx }
    }

    /// Waits until the observed actor and every actor below it have ended, and
    /// returns [`SubtreeEnd::Ended`]. When they have ended already it returns
    /// at once, with no timer. When it returns `Ended`, the names and ids
    /// those actors held are free.
    ///
    /// The wait has two parts. The first is the actor's own end: it goes on
    /// once the actor has ended and let go of everything it held, its hold on
    /// its subtree last, which is after [`ActorHandle::wait_stopped`] would
    /// have returned. It needs no timer and has no bound of its own: wrap the
    /// call in `tokio::time::timeout` to bound it. The
    /// second is the subtree, bounded from the moment the call sees the
    /// actor's end by the longest chain of stop budgets below the actor (a
    /// worker's kill grace, a supervisor child's `Shutdown` window plus its
    /// kill grace, and for a worker that has called `trap_exit` its own
    /// `Shutdown` window as well), each followed by a short settle time, plus
    /// one more settle time. Past that bound the wait gives no verdict on an
    /// actor before the abort the system owes it has had the settle time to
    /// end it. The bound is absent while a supervisor child under
    /// [`Shutdown::Infinity`](crate::types::Shutdown::Infinity) runs below the
    /// actor, or a worker under `Infinity` that traps exit signals (from its
    /// call to `trap_exit` until it ends), whose orderly stop has none, and
    /// when a budget is too large to count (a kill grace of `Duration::MAX`, say): the second part then ends
    /// only when the actors below do. When the bound runs out with an actor
    /// below still running the answer is [`SubtreeEnd::Unresponsive`]; the wait
    /// stops nothing, and a later call can answer `Ended`.
    ///
    /// [`ActorSystem::kill_by_id`] reports the killed actor when it itself
    /// ended, and the actors below it end after it; this wait is how a caller
    /// waits for them.
    ///
    /// The call only reads: dropping it, or the observer, cancels nothing and
    /// leaves nothing owed. Its answer is for the caller alone: it records no
    /// verdict on any supervisor and logs nothing. Called from a callback of
    /// the observed actor or of an actor below it, it waits for the caller's
    /// own end and cannot answer `Ended`.
    ///
    /// # Panics
    /// Panics when the call needs a timer and the calling thread cannot give
    /// it one: the observed actor has ended and let go of everything it held,
    /// an actor below it has not ended, and either no Tokio runtime is
    /// current or the current runtime was built without the time driver
    /// (`Builder::enable_time` or `enable_all`). A runtime that is shutting
    /// down or has ended passes the check, and a call polled by a foreign
    /// executor with such a runtime still entered then panics at its next
    /// timed wait, unless the subtree has ended by then. A call that finds the
    /// actor still running, or still letting go of what it held, waits for it
    /// without a timer, and a call that finds its subtree already ended
    /// answers without one: that wait does not panic, on any runtime or none. With
    /// `panic = "abort"` the check cannot tell whether the runtime has the
    /// time driver, so a call on a runtime without it ends the process at the
    /// first timer it uses.
    ///
    /// # Wakers
    /// Tokio ends its wake of a list of waiting tasks at the waker that
    /// panics, so the tasks behind it in that list may be left unwoken. The
    /// observers of one subtree share a list for the wait on the subtree, and
    /// the wait on the actor's own end shares the actor's status list with
    /// [`ActorHandle::wait_stopped`] and the crate's own waits on the actor. A
    /// `wait_ended` call behind a waker that panics, and a `wait_stopped` call
    /// behind the waker of an observer, may stay pending after the end they
    /// wait for, and return when their task is next polled, not before. Poll an
    /// observer from a task whose waker does not panic.
    ///
    /// # Calling oneself
    /// Called on an observer of one's own incarnation from one of the actor's
    /// own callbacks, the call never completes: the end it waits for cannot
    /// come while the callback waits. It is not refused, since a waiter on
    /// another task is legitimate.
    pub async fn wait_ended(&self) -> SubtreeEnd {
        // The actor's own end comes first and needs no timer. The plane
        // closes once the task has dropped what it owned, and the actor's
        // hold on its subtree is released a step after it, so the bound below
        // never counts time the actor's own end took.
        let mut rx = self.status_rx.clone();
        wait_closed(&mut rx).await;
        // The link is read after the end, never when the observer is built:
        // the registry shows an actor before its subtree is linked, and the
        // link is set before the task is spawned, so a task that ended has one
        // unless it never ran.
        let Some(watch) = self
            .status_rx
            .subtree_watch()
            .map(SubtreeWatch::for_observer)
        else {
            return SubtreeEnd::Ended;
        };
        // The hold is the last thing the actor lets go of. Until it is
        // released the subtree cannot be told from one with an actor still
        // running below, and the answer for an actor with nothing below it
        // needs no timer, so the wait goes on without one.
        watch.released().await;
        if watch.is_gone() {
            return SubtreeEnd::Ended;
        }
        // The timer check comes at the first moment a timer is needed, after
        // everything that needs none has been answered.
        crate::time_probe::require_caller_timers("SubtreeObserver::wait_ended");
        let mut wait = SubtreeWait::new(&watch);
        loop {
            match successor_wait_step(wait.gone(), false, wait.expired()) {
                WaitAction::Keep => wait.changed().await,
                WaitAction::Escalate => return SubtreeEnd::Unresponsive,
                // A cancel needs an order, and this wait never has one.
                WaitAction::Proceed | WaitAction::Cancel => return SubtreeEnd::Ended,
            }
        }
    }
}
