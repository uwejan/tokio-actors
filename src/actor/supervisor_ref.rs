//! A typed reference to a supervisor, for managing its children from outside
//! its callbacks, over the supervisor's system channel.
//!
//! Erlang/OTP's `supervisor` API is a set of `gen_server:call`s on the
//! supervisor, so any process that holds a reference can start, stop, restart
//! and delete children and list them. `SupervisorRef` is that reference: it
//! is made from the supervisor's handle, and each call is a request on the
//! system channel that the supervisor's run loop answers at a boundary, ahead
//! of its mailbox. The supervisor's own callbacks use the methods of
//! `ActorContext` instead.

use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::ops::ControlFlow;
use std::pin::Pin;

use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::actor::context::{ActorContext, ChildSpawnBuilder, JoinTicket};
use crate::actor::handle::ActorHandle;
use crate::actor::panic::{
    catch_hook_t, catch_sync, payload_into_string, send_contained, AnswerHold,
};
use crate::actor::subtree::AbortRecord;
use crate::actor::supervision::{
    await_cell, await_subtree, rules, CellOutcome, ManualStop, Predecessor,
};
use crate::actor::Actor;
use crate::error::{ActorError, SupervisionError, SupervisorRefError};
use crate::helper::HelperKind;
use crate::telemetry::HookKind;
use crate::time_probe::runtime_has_timers;
use crate::types::turn_clock::{outcome_plain, push_turn};
use crate::types::{ActorId, ChildCounts, ChildInfo, StopLane, StopReason, SuperviseRequest};

/// A reference to a supervisor of type `S`, with the calls of Erlang/OTP's
/// `supervisor` module.
///
/// Made from the supervisor's handle with [`of`](Self::of); cloning is cheap
/// and every clone reaches the same run of the supervisor. The reference
/// names one incarnation: a supervisor that was restarted is reached by a
/// reference made from its new handle.
///
/// Each call is answered at a boundary of the supervisor's run loop, so it
/// does not queue behind the supervisor's mailbox and is answered while the
/// mailbox is full. It is not answered while a callback of the supervisor
/// runs, so a call made from the supervisor's own callbacks, or from a task
/// the callback awaits, would wait for itself: a call from the supervisor's
/// own task returns [`SupervisorRefError::CallingSelf`] at once and sends
/// nothing, and a task spawned from a callback that is awaited by that
/// callback parks as any request to a busy actor does.
///
/// Any holder of a supervisor's handle can make a reference to it and manage
/// its children, as any holder of a process id can in Erlang/OTP, and as any
/// holder of a handle can already stop the actor.
///
/// A call waits for its answer without a bound, as Erlang/OTP's `supervisor`
/// calls do; [`timeout`](Self::timeout) bounds every call made through a
/// reference.
///
/// # Examples
/// ```rust
/// use tokio_actors::{
///     actor::{context::ActorContext, Actor, ActorExt},
///     ActorResult, SupervisorRef,
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
/// struct Boss;
/// impl Actor for Boss {
///     type Message = ();
///     type Response = ();
///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
///         Ok(())
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let boss = Boss.spawn().named("sref-type-doc").supervisor().await?;
/// let sup = SupervisorRef::of(&boss);
/// sup.start_child(|| Worker, |child| child.named("sref-type-doc-worker")).await?;
/// assert_eq!(sup.count_children().await?.workers, 1);
/// # Ok(())
/// # }
/// ```
pub struct SupervisorRef<S: Actor> {
    id: ActorId,
    lane: StopLane,
    /// The bound of every call made through this reference, if one was set.
    timeout: Option<Duration>,
    _supervisor: PhantomData<fn() -> S>,
}

impl<S: Actor> Clone for SupervisorRef<S> {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            lane: self.lane.clone(),
            timeout: self.timeout,
            _supervisor: PhantomData,
        }
    }
}

impl<S: Actor> std::fmt::Debug for SupervisorRef<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorRef")
            .field("supervisor", &self.id)
            .finish_non_exhaustive()
    }
}

/// The typed job of a `start_child`: it runs on the supervisor's task with
/// the supervisor's context, and sends its own answer. The request carries it
/// erased, and `serve` opens it at one site.
type StartJob<S> = Box<
    dyn for<'a> FnOnce(&'a mut ActorContext<S>) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
        + Send,
>;

impl<S: Actor> SupervisorRef<S> {
    /// Makes a reference to the supervisor `handle` reaches.
    ///
    /// The reference does not make the actor a supervisor: a call made to an
    /// actor with no supervision configured answers
    /// [`SupervisionError::NotASupervisor`] (or
    /// [`SpawnError::NotASupervisor`](crate::error::SpawnError::NotASupervisor) for `start_child`).
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, SupervisorRef,
    /// };
    ///
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().named("sref-of-doc").supervisor().await?;
    /// let sup = SupervisorRef::of(&boss);
    /// let copy = sup.clone();
    /// assert_eq!(copy.count_children().await?.specs, 0);
    /// # Ok(())
    /// # }
    /// ```
    pub fn of(handle: &ActorHandle<S>) -> Self {
        Self {
            id: handle.id().clone(),
            lane: handle.stop_lane(),
            timeout: None,
            _supervisor: PhantomData,
        }
    }

    /// Bounds every call made through the returned reference (and its clones)
    /// by `within`, which covers the whole call: the wait for room on the
    /// supervisor's system channel as much as the wait for the answer. A call
    /// that is not answered in time answers [`SupervisorRefError::Timeout`],
    /// and one made where the calling runtime has no time driver answers
    /// [`SupervisorRefError::NoTimeDriver`] before anything is sent (with
    /// `panic = "abort"` the check cannot tell a runtime that lacks the
    /// driver, and the call ends the process there, as the first timer of the
    /// crate does on such a runtime). A
    /// reference made with [`of`](Self::of) has no bound, as Erlang/OTP's
    /// `supervisor` calls have none.
    ///
    /// A bound only ends the caller's wait, as dropping the call's future
    /// does, and changes nothing the supervisor does. A request that was
    /// queued before the bound passed is still served: `start_child` still
    /// starts the child and drops its handle, `terminate_child` still stops
    /// the child, and the other calls are applied and their answers dropped.
    /// A child that then outlasts the whole of its stop leaves an
    /// unresponsive verdict with no caller to receive it, and the supervisor
    /// ends with [`SupervisionError::ChildUnresponsive`]; a caller that waits
    /// without a bound would have received that error instead.
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, SupervisorRef,
    /// };
    ///
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().named("sref-timeout-doc").supervisor().await?;
    /// let sup = SupervisorRef::of(&boss).timeout(Duration::from_secs(5));
    /// assert_eq!(sup.count_children().await?.specs, 0);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn timeout(mut self, within: Duration) -> Self {
        self.timeout = Some(within);
        self
    }

    /// Runs `call` under the bound of this reference, if it has one. A call
    /// from the supervisor's own task is refused before the bound is read.
    async fn bounded<T>(
        &self,
        call: impl Future<Output = Result<T, SupervisorRefError>>,
    ) -> Result<T, SupervisorRefError> {
        self.refuse_self_call()?;
        let Some(within) = self.timeout else {
            return call.await;
        };
        if !Handle::try_current().is_ok_and(|runtime| runtime_has_timers(&runtime)) {
            return Err(SupervisorRefError::NoTimeDriver);
        }
        let deadline = crate::actor::runtime::saturating_deadline(Instant::now(), within);
        tokio::time::timeout_at(deadline, call)
            .await
            .unwrap_or(Err(SupervisorRefError::Timeout))
    }

    /// Refuses a call made from the supervisor's own task: nothing could
    /// answer it.
    fn refuse_self_call(&self) -> Result<(), SupervisorRefError> {
        if self.lane.abort_record().is_self_call() {
            return Err(SupervisorRefError::CallingSelf);
        }
        Ok(())
    }

    /// Queues `request` and waits for its answer. A closed plane or an
    /// answer that never comes (the supervisor ended with the request
    /// queued, or the reply helper was lost) is `Gone`.
    async fn ask<T>(
        &self,
        request: impl FnOnce(oneshot::Sender<T>) -> SuperviseRequest,
    ) -> Result<T, SupervisorRefError> {
        let (tx, rx) = oneshot::channel();
        self.lane
            .abort_record()
            .submit_supervise(request(tx))
            .await
            .map_err(|()| SupervisorRefError::Gone)?;
        rx.await.map_err(|_| SupervisorRefError::Gone)
    }

    /// Starts a supervised child, as
    /// [`ActorContext::spawn_child`] does, and
    /// returns its handle once the child is registered.
    ///
    /// `configure` receives the [`ChildSpawnBuilder`] of the child and
    /// returns it with its options set (name, restart type, shutdown policy,
    /// start timeout, kill grace, runtime), so the options are the ones
    /// `spawn_child` has. The factory and the closure run on the supervisor's
    /// task, between its callbacks. The call waits for the child's
    /// registration, for the end of the subtree of a removed child of the
    /// same id and, when `configure` calls
    /// [`await_init`](ChildSpawnBuilder::await_init), for the child's
    /// initialization (bounded by the child's `start_timeout` when it has
    /// one). It holds the supervisor for as long as
    /// `spawn_child(..).await_init()` holds it, and for a child that does not
    /// await its initialization, for as long as `spawn_child` does. A panic
    /// in the factory or the closure is a panic of the supervisor's task: the
    /// supervisor ends with it as with a panic of a handler, and the call
    /// answers [`SupervisorRefError::Gone`].
    ///
    /// Dropping the returned future after the request was queued does not
    /// cancel it: the supervisor starts the child and the handle is dropped.
    ///
    /// # Errors
    /// - [`SupervisorRefError::CallingSelf`] when called from the
    ///   supervisor's own task.
    /// - [`SupervisorRefError::Gone`] when the supervisor ended before it
    ///   answered.
    /// - [`SupervisorRefError::Timeout`] and
    ///   [`SupervisorRefError::NoTimeDriver`] when the reference has a bound
    ///   (see [`timeout`](Self::timeout)).
    /// - [`SupervisorRefError::Spawn`] with the error `spawn_child` has
    ///   ([`SpawnError::DuplicateChild`](crate::error::SpawnError::DuplicateChild) for an id the supervisor holds a
    ///   spec for, [`SpawnError::NotASupervisor`](crate::error::SpawnError::NotASupervisor) for an actor with no
    ///   supervision configured).
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, RestartType, SupervisorRef,
    /// };
    ///
    /// struct Worker;
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = u8;
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<u8> {
    ///         Ok(7)
    ///     }
    /// }
    ///
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().named("sref-start-doc").supervisor().await?;
    /// let sup = SupervisorRef::of(&boss);
    /// let worker = sup
    ///     .start_child(|| Worker, |child| {
    ///         child.named("sref-start-doc-worker").restart_type(RestartType::Transient)
    ///     })
    ///     .await?;
    /// assert_eq!(worker.send(()).await?, 7);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_child<C, F, G>(
        &self,
        factory: F,
        configure: G,
    ) -> Result<ActorHandle<C>, SupervisorRefError>
    where
        C: Actor,
        F: Fn() -> C + Send + Sync + 'static,
        G: for<'a> FnOnce(ChildSpawnBuilder<'a, S, F, C>) -> ChildSpawnBuilder<'a, S, F, C>
            + Send
            + 'static,
    {
        self.bounded(self.start(factory, configure)).await
    }

    /// The request of [`start_child`](Self::start_child), without a bound.
    async fn start<C, F, G>(
        &self,
        factory: F,
        configure: G,
    ) -> Result<ActorHandle<C>, SupervisorRefError>
    where
        C: Actor,
        F: Fn() -> C + Send + Sync + 'static,
        G: for<'a> FnOnce(ChildSpawnBuilder<'a, S, F, C>) -> ChildSpawnBuilder<'a, S, F, C>
            + Send
            + 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<ActorHandle<C>, SupervisorRefError>>();
        // The hold is made before the job exists, and the job's closure and
        // then its future carry it: the sender drops inside the catch when
        // the factory or the configure closure panics, when the queued job
        // is dropped unrun and when the future is dropped before its first
        // poll, so the caller's waker cannot panic a second time in an
        // unwind.
        let reply = AnswerHold::new(Some(tx));
        let job: StartJob<S> = Box::new(move |ctx: &mut ActorContext<S>| {
            Box::pin(async move {
                let mut reply = reply;
                let builder = configure(ctx.spawn_child(factory));
                let started = builder.await.map_err(SupervisorRefError::Spawn);
                // The child is registered whether or not the caller still
                // waits; a handle nobody receives is dropped here.
                if let Some(tx) = reply.take() {
                    drop(send_contained(tx, started));
                }
            })
        });
        self.lane
            .abort_record()
            .submit_supervise(SuperviseRequest::Start(Box::new(job)))
            .await
            .map_err(|()| SupervisorRefError::Gone)?;
        rx.await.map_err(|_| SupervisorRefError::Gone)?
    }

    /// Stops a child without restarting it, as
    /// [`ActorContext::terminate_child`] does,
    /// and answers once the child and every actor it spawned have ended.
    ///
    /// The supervisor commits the stop at a boundary and then serves its
    /// other work: it does not wait for the child, which Erlang/OTP's
    /// `terminate_child` does. The child's end is classified and reported
    /// once to
    /// [`on_child_stopped`](crate::actor::Actor::on_child_stopped) by the
    /// supervisor, whether or not the caller still waits, and the
    /// classification is finished before the answer goes out, so a
    /// [`delete_child`](Self::delete_child) or
    /// [`restart_child`](Self::restart_child) made after the answer finds the
    /// child stopped. Dropping the returned future after the request was
    /// queued does not cancel the stop; a child that then outlasts its stop,
    /// when the caller is gone and cannot be told, ends the supervisor with
    /// [`SupervisionError::ChildUnresponsive`]. A call that answers
    /// [`Timeout`](SupervisorRefError::Timeout) after its request was queued
    /// leaves its stop running in the same way (one whose bound passed while
    /// it still waited for room sent nothing, and no stop begins): a child
    /// that then outlasts the whole of its stop ends the
    /// supervisor, where a caller that waited without a bound would have
    /// received that error.
    ///
    /// # Errors
    /// - [`SupervisorRefError::CallingSelf`] and [`SupervisorRefError::Gone`]
    ///   as for every call; [`SupervisorRefError::Timeout`] and
    ///   [`SupervisorRefError::NoTimeDriver`] when the reference has a bound
    ///   (see [`timeout`](Self::timeout)). The supervisor ending before the child's end was
    ///   seen is `Gone`; once the child's end was seen, the stop completed and
    ///   the answer is `Ok` even if the supervisor ended before it finished
    ///   the classification.
    /// - [`SupervisorRefError::Supervision`] with the error
    ///   `terminate_child` has: [`SupervisionError::NotASupervisor`],
    ///   [`SupervisionError::ChildNotFound`],
    ///   [`SupervisionError::ChildRestarting`] while a restart attempt is in
    ///   flight, and [`SupervisionError::ChildUnresponsive`] when the child
    ///   or an actor it spawned outlasts its stop windows.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, SupervisorRef,
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
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().named("sref-term-doc").supervisor().await?;
    /// let sup = SupervisorRef::of(&boss);
    /// let worker = sup
    ///     .start_child(|| Worker, |child| child.named("sref-term-doc-worker"))
    ///     .await?;
    /// sup.terminate_child(worker.id().clone()).await?;
    /// sup.delete_child(worker.id().clone()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn terminate_child(
        &self,
        child: impl Into<ActorId>,
    ) -> Result<(), SupervisorRefError> {
        let child = child.into();
        self.bounded(self.ask(|reply| SuperviseRequest::Terminate { child, reply }))
            .await?
    }

    /// Revives a child stopped with
    /// [`terminate_child`](Self::terminate_child), as
    /// [`ActorContext::restart_child`] does.
    ///
    /// # Errors
    /// - [`SupervisorRefError::CallingSelf`] and [`SupervisorRefError::Gone`]
    ///   as for every call; [`SupervisorRefError::Timeout`] and
    ///   [`SupervisorRefError::NoTimeDriver`] when the reference has a bound
    ///   (see [`timeout`](Self::timeout)).
    /// - [`SupervisorRefError::Supervision`] with the error `restart_child`
    ///   has ([`SupervisionError::ChildRunning`],
    ///   [`SupervisionError::ChildRestarting`],
    ///   [`SupervisionError::ChildNotFound`], [`SupervisionError::NotASupervisor`]).
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, SupervisorRef,
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
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().named("sref-restart-doc").supervisor().await?;
    /// let sup = SupervisorRef::of(&boss);
    /// let worker = sup
    ///     .start_child(|| Worker, |child| child.named("sref-restart-doc-worker"))
    ///     .await?;
    /// sup.terminate_child(worker.id().clone()).await?;
    /// sup.restart_child(worker.id().clone()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn restart_child(&self, child: impl Into<ActorId>) -> Result<(), SupervisorRefError> {
        let child = child.into();
        self.bounded(self.ask(|reply| SuperviseRequest::Restart { child, reply }))
            .await?
            .map_err(SupervisorRefError::Supervision)
    }

    /// Removes the spec of a stopped child, as
    /// [`ActorContext::delete_child`] does.
    ///
    /// # Errors
    /// - [`SupervisorRefError::CallingSelf`] and [`SupervisorRefError::Gone`]
    ///   as for every call; [`SupervisorRefError::Timeout`] and
    ///   [`SupervisorRefError::NoTimeDriver`] when the reference has a bound
    ///   (see [`timeout`](Self::timeout)).
    /// - [`SupervisorRefError::Supervision`] with the error `delete_child`
    ///   has ([`SupervisionError::ChildRunning`],
    ///   [`SupervisionError::ChildRestarting`],
    ///   [`SupervisionError::ChildNotFound`], [`SupervisionError::NotASupervisor`]).
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, SupervisorRef, SupervisorRefError, SupervisionError,
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
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().named("sref-delete-doc").supervisor().await?;
    /// let sup = SupervisorRef::of(&boss);
    /// let worker = sup
    ///     .start_child(|| Worker, |child| child.named("sref-delete-doc-worker"))
    ///     .await?;
    /// // A running child is not deleted.
    /// let refused = sup.delete_child(worker.id().clone()).await;
    /// assert!(matches!(
    ///     refused,
    ///     Err(SupervisorRefError::Supervision(SupervisionError::ChildRunning(_)))
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn delete_child(&self, child: impl Into<ActorId>) -> Result<(), SupervisorRefError> {
        let child = child.into();
        self.bounded(self.ask(|reply| SuperviseRequest::Delete { child, reply }))
            .await?
            .map_err(SupervisorRefError::Supervision)
    }

    /// Lists the supervisor's children, as
    /// [`ActorContext::children`] does.
    ///
    /// # Errors
    /// - [`SupervisorRefError::CallingSelf`] and [`SupervisorRefError::Gone`]
    ///   as for every call; [`SupervisorRefError::Timeout`] and
    ///   [`SupervisorRefError::NoTimeDriver`] when the reference has a bound
    ///   (see [`timeout`](Self::timeout)).
    /// - [`SupervisorRefError::Supervision`] with
    ///   [`SupervisionError::NotASupervisor`] for an actor with no
    ///   supervision configured.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, SupervisorRef,
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
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().named("sref-which-doc").supervisor().await?;
    /// let sup = SupervisorRef::of(&boss);
    /// sup.start_child(|| Worker, |child| child.named("sref-which-doc-worker")).await?;
    /// let children = sup.which_children().await?;
    /// assert_eq!(children.len(), 1);
    /// assert!(!children[0].is_supervisor);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn which_children(&self) -> Result<Vec<ChildInfo>, SupervisorRefError> {
        self.bounded(self.ask(|reply| SuperviseRequest::Which { reply }))
            .await?
            .map_err(SupervisorRefError::Supervision)
    }

    /// Counts the supervisor's children, as Erlang/OTP's
    /// `supervisor:count_children/1` does: the specs, the active children
    /// (those with an incarnation alive; a child being restarted is not
    /// active), and the specs that are supervisors and workers.
    ///
    /// # Errors
    /// - [`SupervisorRefError::CallingSelf`] and [`SupervisorRefError::Gone`]
    ///   as for every call; [`SupervisorRefError::Timeout`] and
    ///   [`SupervisorRefError::NoTimeDriver`] when the reference has a bound
    ///   (see [`timeout`](Self::timeout)).
    /// - [`SupervisorRefError::Supervision`] with
    ///   [`SupervisionError::NotASupervisor`] for an actor with no
    ///   supervision configured.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, SupervisorRef,
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
    /// struct Boss;
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss.spawn().named("sref-count-doc").supervisor().await?;
    /// let sup = SupervisorRef::of(&boss);
    /// sup.start_child(|| Worker, |child| child.named("sref-count-doc-worker")).await?;
    /// let counts = sup.count_children().await?;
    /// assert_eq!((counts.specs, counts.active, counts.workers), (1, 1, 1));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn count_children(&self) -> Result<ChildCounts, SupervisorRefError> {
        self.bounded(self.ask(|reply| SuperviseRequest::Count { reply }))
            .await?
            .map_err(SupervisorRefError::Supervision)
    }
}

/// Serves one request of a `SupervisorRef` at the run loop's system branch.
///
/// The work is synchronous except `Start`, which awaits the child's
/// registration and the retired-subtree wait, as `spawn_child` does in a
/// handler. A panic in the user code the request runs (the factory and the
/// configure closure of a `Start`, the drop of a removed spec's factory) ends
/// the supervisor with `Failure`, as a handler's panic does. Every answer goes
/// out through `send_contained`, and a sender held across user code is held in
/// `AnswerHold`.
pub(crate) async fn serve<S: Actor>(
    ctx: &mut ActorContext<S>,
    request: Box<SuperviseRequest>,
) -> ControlFlow<StopReason> {
    match *request {
        SuperviseRequest::Restart { child, reply } => {
            answer(ctx, reply, |ctx| ctx.restart_child(child))
        }
        SuperviseRequest::Delete { child, reply } => {
            answer(ctx, reply, |ctx| ctx.delete_child(child))
        }
        SuperviseRequest::Which { reply } => answer(ctx, reply, |ctx| {
            ctx.supervision_ref()
                .map(|sup| sup.children_info())
                .ok_or(SupervisionError::NotASupervisor)
        }),
        SuperviseRequest::Count { reply } => answer(ctx, reply, |ctx| {
            ctx.supervision_ref()
                .map(|sup| sup.child_counts())
                .ok_or(SupervisionError::NotASupervisor)
        }),
        SuperviseRequest::Terminate { child, reply } => terminate(ctx, child, reply),
        SuperviseRequest::Settle {
            child,
            incarnation,
            kind,
            reason,
        } => match catch_sync(|| ctx.settle_manual_stop(&child, incarnation, kind, reason)) {
            Ok(()) => ControlFlow::Continue(()),
            Err(payload) => failed(ctx, payload),
        },
        SuperviseRequest::Start(job) => start(ctx, job).await,
    }
}

/// The end of a supervisor whose request ran user code that panicked.
fn failed<S: Actor>(
    ctx: &mut ActorContext<S>,
    payload: crate::actor::panic::PanicPayload,
) -> ControlFlow<StopReason> {
    let err = ActorError::Panic(payload_into_string(payload));
    ctx.record_failure(err.clone());
    ControlFlow::Break(StopReason::Failure(err))
}

/// Runs a synchronous call of the context and sends its result.
fn answer<S: Actor, T>(
    ctx: &mut ActorContext<S>,
    reply: oneshot::Sender<T>,
    work: impl FnOnce(&mut ActorContext<S>) -> T,
) -> ControlFlow<StopReason> {
    let mut reply = AnswerHold::new(Some(reply));
    match catch_sync(|| work(ctx)) {
        Ok(value) => {
            if let Some(tx) = reply.take() {
                drop(send_contained(tx, value));
            }
            ControlFlow::Continue(())
        }
        Err(payload) => failed(ctx, payload),
    }
}

/// Commits the stop of a child at the boundary and hands the wait for its end
/// to a helper of the crate, which answers when the child and its subtree
/// ended. The classification and the report belong to the supervisor's death
/// arm, which finishes them for any stop it finds committed, so the helper
/// and the caller hold no obligation: a helper that is lost, or a caller that
/// is gone, costs the answer and nothing else.
fn terminate<S: Actor>(
    ctx: &mut ActorContext<S>,
    child: ActorId,
    reply: oneshot::Sender<Result<(), SupervisorRefError>>,
) -> ControlFlow<StopReason> {
    let mut reply = AnswerHold::new(Some(reply));
    let committed =
        match catch_sync(|| ctx.manual_stop_commit(child.clone(), ManualStop::Terminate)) {
            Ok(committed) => committed.map(|committed| committed.join),
            Err(payload) => return failed(ctx, payload),
        };
    let ticket = match committed {
        Ok(ticket) => ticket,
        Err(err) => {
            if let Some(tx) = reply.take() {
                drop(send_contained(
                    tx,
                    Err(SupervisorRefError::Supervision(err)),
                ));
            }
            return ControlFlow::Continue(());
        }
    };
    let predecessor = ctx
        .supervision_mut()
        .and_then(|sup| sup.predecessor_of(&child));
    let nothing_to_wait_for =
        ticket.is_none() && predecessor.as_ref().is_none_or(Predecessor::is_gone);
    if nothing_to_wait_for {
        if let Some(tx) = reply.take() {
            drop(send_contained(tx, Ok(())));
        }
        return ControlFlow::Continue(());
    }
    let wait = TerminateWait {
        child,
        ticket,
        predecessor,
        record: ctx.stop_lane().abort_record().clone(),
        reply,
        verdict: false,
    };
    let system = ctx.resolved_system();
    // The helper works for the supervisor: its end is counted among the
    // supervisor's events, through the supervisor's record, which the
    // supervisor's own handle already keeps alive as long.
    let owner = crate::helper::HelperOwner::new(
        ctx.actor_id().clone(),
        Some(ctx.stop_lane().abort_record().clone()),
    );
    let rt = Handle::current();
    drop(
        system.spawn_helper(&rt, HelperKind::SupervisorReply, Some(owner), move |end| {
            end.observe(wait.run())
        }),
    );
    ControlFlow::Continue(())
}

/// What the reply helper of a `terminate_child` holds.
struct TerminateWait {
    child: ActorId,
    ticket: Option<JoinTicket>,
    predecessor: Option<Predecessor>,
    record: AbortRecord,
    reply: AnswerHold<Result<(), SupervisorRefError>>,
    /// The answer is the child's Unresponsive verdict.
    verdict: bool,
}

impl TerminateWait {
    /// Waits for the child's fate, asks the supervisor to finish the
    /// classification of the stop, waits for the child's subtree and
    /// answers.
    ///
    /// The helper's wait on the child's cell stays registered until the
    /// answer was delivered, so the supervisor leaves the child's verdict to
    /// this caller. A verdict whose answer found the caller gone, or whose
    /// helper was lost, is left to the supervisor when the wait is dropped
    /// unreported: its loop is woken, finds the stopping child with a verdict
    /// and no caller, and ends with `ChildUnresponsive`.
    async fn run(mut self) {
        let result = self.wait().await;
        let delivered = match self.reply.take() {
            Some(tx) => send_contained(tx, result).is_ok(),
            None => false,
        };
        if delivered && self.verdict {
            if let Some(ticket) = &self.ticket {
                ticket.cell.mark_reported();
            }
        }
    }

    async fn wait(&mut self) -> Result<(), SupervisorRefError> {
        if let Some(ticket) = self.ticket.as_mut() {
            let incarnation = ticket.incarnation;
            match await_cell(&mut ticket.cell, incarnation).await {
                CellOutcome::Fate(fate) => {
                    // Queued ahead of the answer, so a request the caller
                    // makes after the answer is served after the
                    // classification: the death arm may not have run yet.
                    let settle = SuperviseRequest::Settle {
                        child: self.child.clone(),
                        incarnation,
                        kind: ManualStop::Terminate,
                        reason: fate.reason,
                    };
                    // A supervisor that ended before it took the request has
                    // classified the end itself; the child's end was seen, so
                    // the stop completed.
                    let _ = self.record.submit_supervise(settle).await;
                }
                CellOutcome::Unresponsive => {
                    self.verdict = true;
                    return Err(SupervisorRefError::Supervision(
                        SupervisionError::ChildUnresponsive(self.child.clone()),
                    ));
                }
                CellOutcome::Cancelled | CellOutcome::NoIncarnation => {}
            }
        }
        if let Some(subtree) = &self.predecessor {
            if rules::successor_wait_step(subtree.is_gone(), false, false)
                != rules::WaitAction::Proceed
                && await_subtree(subtree).await == rules::WaitAction::Escalate
            {
                return Err(SupervisorRefError::Supervision(
                    SupervisionError::ChildUnresponsive(self.child.clone()),
                ));
            }
        }
        Ok(())
    }
}

/// Opens the job of a `start_child` and runs it on the supervisor's task.
/// The job is the only value ever erased here, made by
/// [`SupervisorRef::start_child`] for this same supervisor type, and it never
/// reaches user code; one that is not a job of this type is dropped, which
/// drops its answer sender and answers `Gone`.
async fn start<S: Actor>(
    ctx: &mut ActorContext<S>,
    job: Box<dyn Any + Send>,
) -> ControlFlow<StopReason> {
    let Ok(job) = job.downcast::<StartJob<S>>() else {
        return ControlFlow::Continue(());
    };
    let clock = ctx.turn_clock();
    let turn = clock.begin();
    let done = catch_hook_t(clock.caller_token(turn), || (*job)(ctx)).await;
    let took = clock.end_timed(turn);
    push_turn(
        ctx.stop_lane().abort_record(),
        turn,
        took,
        HookKind::Sys,
        outcome_plain(&done),
    );
    match done {
        Ok(()) => ControlFlow::Continue(()),
        Err(payload) => failed(ctx, payload),
    }
}
