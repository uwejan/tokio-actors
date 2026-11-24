//! The calls of the `sys` client.

use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::oneshot;

use super::{DebugEntry, Install, Statistics, Sys, SysError, DEFAULT_SYS_TIMEOUT};
use crate::actor::envelope::SysCommand;
use crate::actor::handle::ActorHandle;
use crate::actor::sys_state::{state_access, InspectOp, ReplaceOp};
use crate::actor::Actor;
use crate::system::wait_final_reason;
use crate::time_probe::runtime_has_timers;
#[cfg(doc)]
use crate::types::StopReason;

impl<A: Actor> std::fmt::Debug for Sys<'_, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sys")
            .field("actor", self.handle.id())
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl<'a, A: Actor> Sys<'a, A> {
    pub(crate) fn new(handle: &'a ActorHandle<A>) -> Self {
        Self {
            handle,
            timeout: DEFAULT_SYS_TIMEOUT,
        }
    }

    /// Sets the bound of the calls made through this value.
    #[must_use]
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Makes one call: queues the command `make` builds around a reply
    /// channel on the actor's system channel and waits for the reply.
    ///
    /// The runtime of the caller must have a time driver, which the bound
    /// needs, and the bound covers the whole call: a wait for room on a full
    /// channel as much as the wait for the reply. An actor that ended before
    /// it served the command (the channel closed, or the command was dropped
    /// with it) answers [`SysError::ActorEnded`] with the reason the actor
    /// ended with, whichever way the end was reached. A call from the actor's
    /// own task is refused before anything is queued.
    pub(crate) async fn call<T: Send + 'static>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> SysCommand,
    ) -> Result<T, SysError> {
        if self.handle.sys_record().is_self_call() {
            return Err(SysError::CallingSelf);
        }
        if !Handle::try_current().is_ok_and(|runtime| runtime_has_timers(&runtime)) {
            return Err(SysError::NoTimeDriver);
        }
        let record = self.handle.sys_record();
        let (reply, answer) = oneshot::channel();
        let command = make(reply);
        let served = async {
            let queued = record.submit_sys(command).await;
            match queued {
                Ok(()) => match answer.await {
                    Ok(value) => Ok(value),
                    Err(_) => Err(self.ended().await),
                },
                Err(()) => Err(self.ended().await),
            }
        };
        match tokio::time::timeout(self.timeout, served).await {
            Ok(outcome) => outcome,
            Err(_) => Err(SysError::Timeout),
        }
    }

    /// The error for a call the actor did not serve because it ended: the
    /// reason it ended with, read from the status plane once it is final.
    async fn ended(&self) -> SysError {
        let mut rx = self.handle.status_rx();
        SysError::ActorEnded(wait_final_reason(&mut rx).await)
    }

    /// Stops the actor from taking messages until [`resume`](Self::resume).
    ///
    /// A suspended actor takes nothing from its mailbox or its priority lane.
    /// A supervisor handles no child death, restart outcome or verdict, so it
    /// restarts nothing; what arrives waits and is taken in the loop's usual
    /// order after `resume`. The actor still answers
    /// [`get_status`](ActorHandle::get_status) and every call of this type,
    /// a stop request still runs `pre_stop` and ends the actor (a supervisor
    /// stops its children as it always does), and a `Kill` ends it at once.
    ///
    /// Senders, timers and streams go on putting messages in the mailbox, so
    /// a long suspension fills it and senders wait as they do for an actor
    /// that is busy: bound a send with `send_timeout`.
    ///
    /// Suspending twice is the same as once, and one `resume` ends it. No
    /// wait of the library depends on a suspension, so its owner is the
    /// caller that suspended: a forgotten suspension of a supervisor holds
    /// its restarts back while the restart budget's window keeps running.
    /// Dropping every handle never ends an actor, so one whose handles are
    /// all gone while it is suspended stays suspended and is found by its id
    /// ([`ActorSystem::get_by_id`](crate::ActorSystem::get_by_id)) until a
    /// stop, a shutdown or a `Kill` ends it, or a `resume` through a handle
    /// found that way ends the suspension. A restart
    /// makes a new incarnation, which is not suspended. The state is
    /// reported by [`ActorStatusInfo::suspended`](crate::ActorStatusInfo) and
    /// by the system's snapshot.
    pub async fn suspend(&self) -> Result<(), SysError> {
        self.call(SysCommand::Suspend).await
    }

    /// Ends a suspension. The actor takes up what waited in the loop's usual
    /// order. On an actor that is not suspended it changes nothing.
    pub async fn resume(&self) -> Result<(), SysError> {
        self.call(SysCommand::Resume).await
    }

    /// Turns the statistics on or off.
    pub async fn set_statistics(&self, on: bool) -> Result<(), SysError> {
        self.call(|reply| SysCommand::SetStatistics(on, reply))
            .await
    }

    /// The statistics, or `None` while they are off.
    pub async fn statistics(&self) -> Result<Option<Statistics>, SysError> {
        self.call(SysCommand::Statistics).await
    }

    /// Turns the log on with room for `capacity` entries, or off with `None`.
    pub async fn set_log(&self, capacity: Option<usize>) -> Result<(), SysError> {
        self.call(|reply| SysCommand::SetLog(capacity, reply)).await
    }

    /// The log, oldest entry first, or `None` while it is off.
    pub async fn log(&self) -> Result<Option<Vec<DebugEntry>>, SysError> {
        self.call(SysCommand::Log).await
    }

    /// Turns the trace on or off.
    pub async fn set_trace(&self, on: bool) -> Result<(), SysError> {
        self.call(|reply| SysCommand::SetTrace(on, reply)).await
    }

    /// Installs `f` under `id`, replacing a function installed under the same
    /// id. `f` runs on the actor's task for each message-in and message-out
    /// entry, not for the entry of the stop. A function that panics is
    /// removed; with the `tracing` feature the panic is logged, naming the
    /// actor and the id. With `panic = "abort"` a panic in `f` ends the
    /// process.
    pub async fn install(
        &self,
        id: impl Into<String>,
        f: impl FnMut(&DebugEntry) -> Install + Send + 'static,
    ) -> Result<(), SysError> {
        let id = id.into();
        self.call(|reply| SysCommand::Install(id, Box::new(f), reply))
            .await
    }

    /// Removes the function installed under `id`; true when there was one.
    pub async fn remove(&self, id: &str) -> Result<bool, SysError> {
        let id = id.to_owned();
        self.call(|reply| SysCommand::Remove(id, reply)).await
    }

    /// Reads the actor's value with `f` and returns the result.
    ///
    /// `f` runs on the actor's task between two turns, as a turn the stuck-turn
    /// watchdog sees, and also while the actor is suspended. The actor must
    /// have been spawned with
    /// [`allow_state_access`](crate::actor::SpawnBuilder::allow_state_access)
    /// and must not be sensitive. Erlang/OTP's `sys:get_state` works on any
    /// process; this crate closes state access by default so that an actor's
    /// value stays encapsulated.
    ///
    /// # Errors
    /// - [`SysError::NotAllowed`] if the actor was not spawned with state
    ///   access.
    /// - [`SysError::Sensitive`] if the actor is sensitive.
    /// - [`SysError::Callback`] if `f` panicked; the actor goes on. A panic
    ///   leaves state behind interior mutability as it was when the panic
    ///   struck.
    /// - [`SysError::Timeout`] if the actor did not serve the call within the
    ///   bound of this `Sys`; a request whose caller has gone is not run, but
    ///   one that is running when the bound passes runs to its end.
    /// - [`SysError::ActorEnded`] if the actor ended first.
    /// - [`SysError::NoTimeDriver`] if the calling runtime has no time driver.
    /// - [`SysError::CallingSelf`] if the call is made from the actor's own
    ///   task.
    ///
    /// With `panic = "abort"` a panic in `f` ends the process, so there is no
    /// `Callback` error.
    pub async fn inspect<R: Send + 'static>(
        &self,
        f: impl FnOnce(&A) -> R + Send + 'static,
    ) -> Result<R, SysError> {
        state_access(self.handle.sys_record())?;
        self.call(|reply| SysCommand::State(Box::new(InspectOp::<A, R, _>::new(f, reply))))
            .await?
    }

    /// Changes the actor's value with `f`.
    ///
    /// `f` runs on the actor's task between two turns, as a turn the stuck-turn
    /// watchdog sees, and also while the actor is suspended. The actor must
    /// have been spawned with
    /// [`allow_state_access`](crate::actor::SpawnBuilder::allow_state_access)
    /// and must not be sensitive.
    ///
    /// # Errors
    /// - [`SysError::NotAllowed`] and [`SysError::Sensitive`] as for
    ///   [`inspect`](Self::inspect).
    /// - [`SysError::Callback`] if `f` panicked. The value may be half
    ///   changed, so the actor ends with a
    ///   [`StopReason::Failure`] carrying the panic; Erlang/OTP's
    ///   `sys:replace_state` keeps the old state when the function fails. With
    ///   `panic = "abort"` a panic in `f` ends the process instead.
    /// - [`SysError::Timeout`], [`SysError::ActorEnded`],
    ///   [`SysError::NoTimeDriver`] and [`SysError::CallingSelf`] as for
    ///   [`inspect`](Self::inspect). A change that answers `Timeout` may have
    ///   been applied: `f` runs to its end when it had begun.
    pub async fn replace_state(
        &self,
        f: impl FnOnce(&mut A) + Send + 'static,
    ) -> Result<(), SysError> {
        state_access(self.handle.sys_record())?;
        self.call(|reply| SysCommand::State(Box::new(ReplaceOp::<A, _>::new(f, reply))))
            .await?
    }
}
