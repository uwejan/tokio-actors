//! The spawn builder and `ActorExt::spawn`.

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use super::context::ActorContext;
use super::runtime::ActorConfig;
use super::supervision::SupervisionConfig;
#[cfg(feature = "sys")]
use super::sys::DebugOptions;
use super::{
    closed_ack_error, Actor, ActorExt, LinkedSpawn, MonitoredSpawn, SpawnAck, SpawnBuilder,
};
#[cfg(doc)]
use crate::actor::sys;
use crate::actor::{handle, runtime, start_bound};
use crate::error::SpawnError;
use crate::system::ActorSystem;
use crate::telemetry::Birth;
use crate::types::{ActorId, Down, MonitorId, StopReason};

impl<A: Actor> std::fmt::Debug for SpawnBuilder<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnBuilder")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl<A: Actor> SpawnBuilder<A> {
    /// Assigns a name to the actor. The name also serves as the [`ActorId`].
    /// Named actors are automatically registered in the actor system.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Targets a specific [`ActorSystem`] for registration.
    pub fn on_system(mut self, system: &Arc<ActorSystem>) -> Self {
        self.system = Some(Arc::clone(system));
        self
    }

    /// Overrides the default [`ActorConfig`]. A supervisor role chosen on
    /// this builder with [`supervisor`](Self::supervisor) or
    /// [`with_supervision`](Self::with_supervision) before this call stays,
    /// unless `config` carries a supervision config of its own, which then
    /// wins; a role chosen after this call replaces whatever `config`
    /// carried.
    pub fn with_config(mut self, config: ActorConfig) -> Self {
        let role = self.config.supervision.take();
        self.config = config;
        if self.config.supervision.is_none() {
            self.config.supervision = role;
        }
        self
    }

    /// Makes this actor a SUPERVISOR with the default config (OneForOne,
    /// 3 restarts / 5s window). A supervisor can spawn supervised children via
    /// [`ActorContext::spawn_child`](crate::actor::context::ActorContext::spawn_child).
    ///
    /// The actor takes the supervisor role; its children are the supervised
    /// ones.
    pub fn supervisor(mut self) -> Self {
        self.config = self.config.supervisor();
        self
    }

    /// Enables supervision with a custom [`SupervisionConfig`].
    pub fn with_supervision(mut self, sup: SupervisionConfig) -> Self {
        self.config = self.config.with_supervision(sup);
        self
    }

    /// Skips the initialization ack entirely: `.await` returns as soon as the
    /// task is spawned, before `pre_start` ever runs. An init failure is
    /// then silent to this caller: the actor
    /// still stops, but nobody here is told why. This does not change how a
    /// supervised child reports init failure to its parent (the runner and
    /// restart budget already cover that); it only changes what an
    /// unsupervised, top-level caller of `.spawn()` observes.
    pub fn detached(mut self) -> Self {
        self.ack = SpawnAck::Detached;
        self
    }

    /// Bounds how long `.await` waits for `pre_start`/`on_started` to finish. A
    /// timeout beyond 30 years is treated as 30 years.
    ///
    /// On expiry the system cuts the start, as Erlang/OTP kills a process
    /// whose start times out: it raises `Kill` on the actor's stop lane and
    /// requests an abort of the task. The abort takes effect when the task
    /// next suspends, and the task is dropped there. The `Kill` is read before
    /// each start hook and at each turn boundary, so once the start has timed
    /// out no hook begins and no message is handled; a hook that is already
    /// running finishes first if it never yields, and the actor then ends with
    /// [`StopReason::Kill`]. Before the caller gets
    /// [`SpawnError::StartTimeout`], this bounded-waits for the aborted task's
    /// own teardown (including its registry guard drop) to finish, so an
    /// immediate retry under the same name does not race a predecessor whose
    /// teardown ends within that short wait.
    ///
    /// The bound holds whether or not the caller keeps waiting: the cut is
    /// carried out by a task of the system on the actor's runtime, so a caller
    /// that drops the future, or stores it and never polls it again, still
    /// finds the actor cut once the timeout has passed if its acknowledgment
    /// was not ready when the system's task checked for it, and its name free
    /// as soon as the task suspends or reads the `Kill`, whichever comes first:
    /// the system waits at most 100 ms for that, and a task that does neither
    /// in that time, one that never yields included, keeps its name longer. A
    /// caller is told [`SpawnError::StartTimeout`] only for an actor whose
    /// start the system cut, or when the system's task ended without deciding
    /// and the actor's end was not final within the abort settle time after the
    /// deadline; the handle is returned only for an actor that runs.
    ///
    /// The caller only waits for the system's answer and has no bound of its
    /// own. The task of the system runs on the caller's runtime, which is the
    /// runtime the actor runs on, so the call needs that runtime to be driven,
    /// not only entered. If that task panics before it answers, the start is
    /// cut before the answer channel closes and the caller is told
    /// `StartTimeout`, unless the task had already accepted the actor's
    /// acknowledgment: then the actor keeps running and the caller receives the
    /// handle. If the runtime ends first, the actor ends with it and nothing is
    /// left to abort.
    ///
    /// OTP parity: `proc_lib:start` with a timeout (`sync_start` in
    /// `proc_lib.erl`) kills the new process in the caller's own receive once
    /// the timeout passes. A caller that died first is covered only by the link
    /// `start_link` makes, which `proc_lib:start` does not. Here the system
    /// takes that step, so it holds whether or not the caller goes on waiting,
    /// and its wait for the aborted task's teardown is at most 100 ms where OTP
    /// waits for the `'DOWN'` without a limit: two declared departures.
    ///
    /// A zero bound acts at once: `Duration::ZERO` spawns the actor, has the
    /// system cut its start as soon as the system's task for the bound runs,
    /// whatever the actor's ack, and answers [`SpawnError::StartTimeout`] once
    /// the abort has settled;
    /// `pre_start` and `on_started` may already have run.
    ///
    /// # Panics
    /// The `.await` panics when it is first polled on a runtime without the
    /// time driver (build it with `Builder::enable_time` or `enable_all`),
    /// before the actor is spawned or registered: the bound sleeps on the
    /// calling runtime's timers. That is the only runtime the check
    /// refuses: a calling runtime that is shutting down or has ended passes
    /// it. A spawn that is refused for a zero mailbox
    /// capacity, a system that is shutting down or a name already taken is
    /// answered with that error first, on any runtime; a name taken between
    /// that look and the registration still panics on a runtime without the
    /// driver.
    /// With no runtime current the spawn returns [`SpawnError::MissingRuntime`]
    /// as any spawn does. A caller polled by a foreign executor whose runtime
    /// has ended before the system answered is answered, with a runtime that
    /// has the time driver current, from what the system had decided (the
    /// handle if the start was acknowledged, `SpawnError::StartTimeout` if it
    /// was cut, otherwise the actor's end as `SpawnError::Killed` or
    /// `SpawnError::Init`, or `SpawnError::StartTimeout` if that end is still
    /// not final when the abort settle time after the deadline has passed) and
    /// waits on that runtime's timers only for an end that is not yet final. A
    /// poll with no runtime context panics, as any Tokio timer does, unless
    /// the start was acknowledged.
    /// With `panic = "abort"` there is no check at the entry: the first
    /// timer the call uses on a runtime without the driver ends the process.
    pub fn start_timeout(mut self, timeout: Duration) -> Self {
        self.ack = SpawnAck::Timeout(timeout);
        self
    }

    /// Sets how long this actor is given, after a `Kill` is raised on it, to
    /// reach its next turn boundary before its task is aborted. Default:
    /// 120 ms.
    ///
    /// `Duration::ZERO` aborts the task as soon as the `Kill` is raised, so a
    /// callback in progress is cut at its next `.await` and never observes
    /// the `Kill` itself. The grace applies to every kill of this actor:
    /// [`ActorSystem::kill`](crate::system::ActorSystem::kill),
    /// [`ActorSystem::kill_by_id`](crate::system::ActorSystem::kill_by_id),
    /// [`ActorHandle::stop`](crate::actor::handle::ActorHandle::stop) with
    /// [`StopReason::Kill`], the kill step of
    /// [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within)
    /// and of system shutdown, its supervisor's stop of it, and the kill a
    /// worker that does not trap exit signals receives when its own supervisor
    /// is killed. An actor that is itself a supervisor, or a worker that traps
    /// exit signals, stops in order when its supervisor is killed,
    /// bounded by its own [`Shutdown`](crate::types::Shutdown), and the
    /// grace applies to the `Kill` raised once that window has passed;
    /// [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within)
    /// called with `StopReason::Kill` waits its own `within` instead, for
    /// that call. A grace beyond 30 years is capped there, so in practice it
    /// never aborts. A callback that never yields survives the abort as well
    /// and is reported unresponsive.
    ///
    /// `kill_grace(Duration::ZERO)` gives a kill that cuts a callback at its
    /// next await, the nearest the runtime offers to Erlang/OTP's immediate
    /// kill; a grace beyond any practical bound keeps every turn
    /// whole, and makes every wait that runs on that grace, its parent's stop
    /// budget included, last for the grace, which in practice is as long as
    /// under `Shutdown::Infinity`, while `stop_within(StopReason::Kill, d)`
    /// still aborts after `d`.
    pub fn kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = grace;
        self
    }

    /// Starts the actor with the given debug options on: statistics, the log
    /// and the trace, as the [`sys`] calls would turn them on. A supervised
    /// child restarts with the same options, and each incarnation starts from
    /// them, not from the toggles of the one before.
    ///
    /// Needs the `sys` feature, which is on by default.
    #[cfg(feature = "sys")]
    pub fn debug(mut self, options: DebugOptions) -> Self {
        self.sys.debug = options;
        self
    }

    /// Lets [`inspect`](sys::Sys::inspect) and
    /// [`replace_state`](sys::Sys::replace_state) reach the actor's value.
    /// State access is closed otherwise. A sensitive actor stays closed.
    ///
    /// Needs the `sys` feature, which is on by default.
    #[cfg(feature = "sys")]
    pub fn allow_state_access(mut self) -> Self {
        self.sys.allow_state_access = true;
        self
    }

    /// Makes this actor sensitive, as [`Actor::SENSITIVE`] does for every
    /// spawn of its type.
    pub fn sensitive(mut self) -> Self {
        self.sys.sensitive = true;
        self
    }

    /// Monitors the actor for `ctx`'s actor from its first moment, as Erlang/OTP's
    /// `spawn_monitor` does: the notice is made as the spawn returns, so an
    /// actor that ends at once is never missed. `.await` resolves to the
    /// handle and the [`MonitorId`], and the notice reaches `ctx`'s actor
    /// through `map`, as for [`ActorContext::monitor`].
    ///
    /// The monitor is made after the start is acknowledged, and a start that
    /// fails has one outcome: the `.await` returns the [`SpawnError`], no
    /// monitor exists and no notice follows. An actor that ends before the
    /// monitor is made still sends its notice with its own reason, never
    /// `NoProc`. A spawn made with
    /// `.detached()` has no acknowledgment to wait for, so its
    /// monitor is made as soon as the task is spawned, and a start that fails
    /// afterwards is reported to `map` as any other end.
    ///
    /// Chain it after the other options (`named`, `on_system`, `with_config`,
    /// `start_timeout`), as the last call before `.await`; add
    /// `.linked()` on the result to link as well.
    ///
    /// A spawn from an actor that is stopping or has stopped is refused with
    /// [`SpawnError::OwnerStopping`] before anything is spawned, because the
    /// notices a stopping actor is owed were fixed when its stop began.
    ///
    /// Only the root builder has this form. A supervised child has none: after
    /// [`ActorContext::spawn_child`] returns, `ctx.monitor(&child, map)` is
    /// the same monitor, and a child whose first start fails still sends its
    /// notice, because that start is not awaited.
    ///
    /// Erlang/OTP: `erlang:spawn_monitor/1`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Down, DownReason, StopReason,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         // The worker's work is done as soon as it starts.
    ///         ctx.stop(StopReason::Graceful).expect("the lane takes the request");
    ///         Ok(())
    ///     }
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Start,
    ///     Gone(Down),
    /// }
    ///
    /// struct Parent(mpsc::UnboundedSender<Down>);
    ///
    /// impl Actor for Parent {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         match msg {
    ///             Msg::Start => {
    ///                 Worker.spawn().monitored(ctx, Msg::Gone).await.expect("the worker starts");
    ///             }
    ///             Msg::Gone(down) => {
    ///                 let _ = self.0.send(down);
    ///             }
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let (tx, mut downs) = mpsc::unbounded_channel();
    /// let parent = Parent(tx).spawn().await?;
    /// parent.notify(Msg::Start).await?;
    /// let down = downs.recv().await.expect("the monitor fires");
    /// assert_eq!(down.reason, DownReason::Stopped(StopReason::Graceful));
    /// # Ok(())
    /// # }
    /// ```
    pub fn monitored<W, F>(self, ctx: &mut ActorContext<W>, map: F) -> MonitoredSpawn<'_, A, W>
    where
        W: Actor,
        F: Fn(Down) -> W::Message + Send + Sync + 'static,
    {
        MonitoredSpawn {
            inner: self,
            ctx,
            map: Box::new(map),
            linked: false,
        }
    }

    /// Links the actor to `ctx`'s actor from its first moment, as Erlang/OTP's
    /// `spawn_link` does: the link is made as the spawn returns, so an actor
    /// that ends at once is never missed, and one that has already ended by
    /// then delivers its own end reason through the link rules, as if the
    /// link had been there before it ran. `.await` resolves to the handle.
    ///
    /// The link is made after the start is acknowledged, and a start that
    /// fails has one outcome: the `.await` returns the [`SpawnError`], no link
    /// exists and no exit signal follows. A spawn made with
    /// `.detached()` has no acknowledgment to wait for, so its
    /// link is made as soon as the task is spawned.
    ///
    /// An actor that ended before `ctx`'s actor resumed from the start
    /// acknowledgment has already sent its exit: [`ActorContext::unlink`]
    /// answers `false` for it and the exit still arrives.
    ///
    /// Chain it after the other options (`named`, `on_system`, `with_config`,
    /// `start_timeout`), as the last call before `.await`. A spawn from an
    /// actor that is stopping or has stopped is refused with
    /// [`SpawnError::OwnerStopping`] before anything is spawned.
    ///
    /// Only the root builder has this form. After
    /// [`ActorContext::spawn_child`] returns, `ctx.link(&child)` is the same
    /// link.
    ///
    /// Erlang/OTP: `erlang:spawn_link/1`.
    pub fn linked<W: Actor>(self, ctx: &mut ActorContext<W>) -> LinkedSpawn<'_, A, W> {
        LinkedSpawn { inner: self, ctx }
    }
}

/// Whether an actor still takes new monitors and links: the notices a
/// stopping actor is owed were fixed when its stop began.
fn owner_takes_registrations<W: Actor>(ctx: &ActorContext<W>) -> bool {
    !matches!(
        ctx.status(),
        crate::types::ActorStatus::Stopping | crate::types::ActorStatus::Stopped
    )
}

impl<A: Actor, W: Actor> std::fmt::Debug for MonitoredSpawn<'_, A, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitoredSpawn")
            .field("inner", &self.inner)
            .field("linked", &self.linked)
            .finish_non_exhaustive()
    }
}

impl<A: Actor, W: Actor> MonitoredSpawn<'_, A, W> {
    /// Links the new actor as well as monitoring it, as Erlang/OTP's
    /// `spawn_opt` with the `link` and `monitor` options does. The link is
    /// made right after the monitor, on the same terms: an actor that has
    /// already ended delivers its own end reason through the link rules, and
    /// [`ActorContext::unlink`] answers `false` for it.
    pub fn linked(mut self) -> Self {
        self.linked = true;
        self
    }
}

impl<'ctx, A: Actor, W: Actor> IntoFuture for MonitoredSpawn<'ctx, A, W> {
    type Output = Result<(handle::ActorHandle<A>, MonitorId), SpawnError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'ctx>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Self {
                inner,
                ctx,
                map,
                linked,
            } = self;
            if !owner_takes_registrations(ctx) {
                return Err(SpawnError::OwnerStopping);
            }
            let handle = inner.await?;
            // The spawning actor's own task is the one awaiting, so its stop
            // stage cannot begin between the check above and these calls.
            let made = ctx.monitor_spawned(&handle, map);
            let made = made.map_err(|_| ()).and_then(|id| {
                if linked {
                    ctx.link_spawned(&handle).map_err(|_| ())?;
                }
                Ok(id)
            });
            match made {
                Ok(id) => Ok((handle, id)),
                Err(()) => {
                    debug_assert!(
                        false,
                        "an actor that took a spawn was refused a registration"
                    );
                    let _ = handle.stop(StopReason::Graceful).await;
                    Err(SpawnError::OwnerStopping)
                }
            }
        })
    }
}

impl<A: Actor, W: Actor> std::fmt::Debug for LinkedSpawn<'_, A, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkedSpawn")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<'ctx, A: Actor, W: Actor> IntoFuture for LinkedSpawn<'ctx, A, W> {
    type Output = Result<handle::ActorHandle<A>, SpawnError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'ctx>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Self { inner, ctx } = self;
            if !owner_takes_registrations(ctx) {
                return Err(SpawnError::OwnerStopping);
            }
            let handle = inner.await?;
            // As for a monitored spawn, the owner's stop stage cannot begin
            // here, so the link is not refused.
            if ctx.link_spawned(&handle).is_err() {
                debug_assert!(
                    false,
                    "an actor that took a spawn was refused a registration"
                );
                let _ = handle.stop(StopReason::Graceful).await;
                return Err(SpawnError::OwnerStopping);
            }
            Ok(handle)
        })
    }
}

impl<A: Actor> IntoFuture for SpawnBuilder<A> {
    type Output = Result<handle::ActorHandle<A>, SpawnError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            // The caller's `String` is copied once into one shared allocation,
            // which the id, the registry key, the guard and the context all
            // hold.
            let name: Option<Arc<str>> = self.name.map(Arc::from);
            let id_str: ActorId = match &name {
                Some(name) => ActorId::from_shared(name.clone()),
                None => ActorId::anonymous(),
            };

            match self.ack {
                SpawnAck::Detached => runtime::into_actor(
                    id_str,
                    self.actor,
                    self.config,
                    name,
                    self.system,
                    self.kill_grace,
                    self.sys,
                ),
                SpawnAck::Await => {
                    let (tx, rx) = oneshot::channel();
                    let (handle, _join, _subtree) = runtime::spawn_actor_in(
                        id_str,
                        self.actor,
                        self.config,
                        name,
                        self.system,
                        Some(tx),
                        true,
                        self.kill_grace,
                        None,
                        None,
                        Birth::root(),
                        self.sys,
                    )?;
                    match rx.await {
                        Ok(Ok(())) => Ok(handle),
                        Ok(Err(err)) => Err(SpawnError::Init(Box::new(err))),
                        // The ack sender dropped without a reply: the actor
                        // stopped before it finished initializing.
                        Err(_) => Err(closed_ack_error(&handle).await),
                    }
                }
                SpawnAck::Timeout(timeout) => {
                    // What needs no timer is answered first; the timer check
                    // comes next, and everything that changes state after it.
                    runtime::spawn_precheck(&self.config, self.system.as_ref(), name.as_deref())?;
                    crate::time_probe::require_timers_if_in_runtime("SpawnBuilder::start_timeout");
                    let requested = self.system.clone();
                    let (tx, rx) = oneshot::channel();
                    let (handle, join, _subtree) = runtime::spawn_actor_in(
                        id_str.clone(),
                        self.actor,
                        self.config,
                        name,
                        self.system,
                        Some(tx),
                        true,
                        self.kill_grace,
                        None,
                        None,
                        Birth::root(),
                        self.sys,
                    )?;
                    let target = requested.unwrap_or_else(ActorSystem::default);
                    // Boxed, so only a timed spawn carries the bound's state.
                    Box::pin(start_bound::bounded_start(
                        &target, id_str, handle, join, rx, timeout,
                    ))
                    .await
                }
            }
        })
    }
}

impl<T> ActorExt for T where T: Actor {}
