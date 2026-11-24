//! Peer monitors: `monitor`, `demonitor` and their priority and flush forms.

use std::sync::Arc;

use super::{ActorContext, ForwarderKind, PanicSlot};
use crate::actor::handle::ActorHandle;
use crate::actor::monitor::{IntoMonitorTarget, MapFn, MonitorState};
use crate::actor::{link, monitor, Actor};
use crate::error::{LinkError, MonitorError};
#[cfg(doc)]
use crate::types::ActorId;
use crate::types::{ActorStatus, Down, MonitorId};

impl<A: Actor> ActorContext<A> {
    // - Monitors -----------------------------------------------------------

    /// Watches an actor: when it ends, `map` turns a [`Down`] into one of this
    /// actor's messages, which arrives through `handle` like any other
    /// message. The target is a handle, an [`ActorId`] or a name (see
    /// [`IntoMonitorTarget`]); an id or a name is resolved once, when the call is made, in this
    /// actor's system.
    ///
    /// Kind: a registration of a notice or an answer the actor then waits for; refused with `MonitorError::Stopping` while the actor is stopping or stopped. See [`signals`](crate::signals).
    ///
    /// The notice is sent once. It names the run of the target the monitor
    /// was bound to ([`Down::incarnation`]), so a supervised restart of the
    /// target fires the monitor with the old run's reason and spends it; watch
    /// the new run with a new monitor. A target that is gone when the call is
    /// made, or that matches no actor, gives a notice with
    /// [`DownReason::NoProc`](crate::types::DownReason::NoProc) at once. Making
    /// several monitors of one target is not an error; each is independent.
    ///
    /// The notice is sent after the target's registered name is free and after
    /// every message the target sent this actor before its reason was final,
    /// because it takes an ordinary mailbox slot behind them. A message that
    /// the target's own value sends from its `Drop`, after the target ended,
    /// may reach this actor after the notice. It may arrive
    /// before the target's own children have ended;
    /// [`SubtreeObserver::wait_ended`](crate::actor::handle::SubtreeObserver::wait_ended)
    /// is the wait for the whole subtree.
    ///
    /// `map` runs on a task of the crate, concurrently with this actor, and
    /// must not block. A panic in it fails this actor with that panic as its
    /// reason. It may be called for a monitor that is revoked at that moment,
    /// and its result is then dropped.
    ///
    /// A notice that is still owed when this actor stops is delivered once,
    /// through `handle`, before `on_stopped`, with the status reading
    /// [`ActorStatus::Stopping`]; none is
    /// delivered when the stop is a `Kill`. A notice already queued in the
    /// mailbox when the stop begins is delivered with those, in the order it
    /// was queued; the other queued messages are dropped as at any stop. A
    /// monitor made while this actor is stopping returns
    /// [`MonitorError::Stopping`].
    ///
    /// Erlang/OTP: `erlang:monitor(process, Target)`, with the `{'DOWN', ...}`
    /// message mapped to a caller-chosen message instead of a fixed tuple.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult, Down, DownReason, StopReason,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Watch(ActorHandle<Worker>),
    ///     Gone(Down),
    /// }
    ///
    /// struct Watcher {
    ///     seen: mpsc::UnboundedSender<Down>,
    /// }
    ///
    /// impl Actor for Watcher {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         match msg {
    ///             Msg::Watch(worker) => {
    ///                 ctx.monitor(&worker, Msg::Gone).expect("the watcher is running");
    ///             }
    ///             Msg::Gone(down) => {
    ///                 let _ = self.seen.send(down);
    ///             }
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let (seen, mut downs) = mpsc::unbounded_channel();
    /// let worker = Worker.spawn().await?;
    /// let watcher = Watcher { seen }.spawn().await?;
    /// watcher.send(Msg::Watch(worker.clone())).await?;
    ///
    /// worker.stop(StopReason::Graceful).await?;
    /// let down = downs.recv().await.expect("the monitor fires");
    /// assert_eq!(down.incarnation, Some(worker.incarnation()));
    /// assert_eq!(down.reason, DownReason::Stopped(StopReason::Graceful));
    /// # Ok(())
    /// # }
    /// ```
    pub fn monitor<T, F>(&mut self, target: T, map: F) -> Result<MonitorId, MonitorError>
    where
        T: IntoMonitorTarget,
        F: Fn(Down) -> A::Message + Send + Sync + 'static,
    {
        self.register_monitor(target, map, false, false)
    }

    /// As [`monitor`](Self::monitor), with the notice entering through the
    /// priority lane, ahead of the ordinary messages queued behind it. An
    /// actor without a priority lane takes it through the mailbox, as
    /// `monitor` does.
    ///
    /// Kind: a registration of a notice or an answer the actor then waits for; refused with `MonitorError::Stopping` while the actor is stopping or stopped. See [`signals`](crate::signals).
    ///
    /// Erlang/OTP: `erlang:monitor(process, Target, [priority])` (OTP 28).
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, runtime::ActorConfig, Actor, ActorExt},
    ///     ActorHandle, ActorResult, Down, StopReason,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Watch(ActorHandle<Worker>),
    ///     Gone(Down),
    /// }
    ///
    /// struct Watcher {
    ///     seen: mpsc::UnboundedSender<Down>,
    /// }
    ///
    /// impl Actor for Watcher {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         match msg {
    ///             Msg::Watch(worker) => {
    ///                 ctx.monitor_priority(&worker, Msg::Gone).expect("the watcher is running");
    ///             }
    ///             Msg::Gone(down) => {
    ///                 let _ = self.seen.send(down);
    ///             }
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let (seen, mut downs) = mpsc::unbounded_channel();
    /// let worker = Worker.spawn().await?;
    /// let watcher = Watcher { seen }
    ///     .spawn()
    ///     .with_config(ActorConfig::default().with_priority_lane(4))
    ///     .await?;
    /// watcher.send(Msg::Watch(worker.clone())).await?;
    ///
    /// worker.stop(StopReason::Graceful).await?;
    /// assert!(downs.recv().await.is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn monitor_priority<T, F>(&mut self, target: T, map: F) -> Result<MonitorId, MonitorError>
    where
        T: IntoMonitorTarget,
        F: Fn(Down) -> A::Message + Send + Sync + 'static,
    {
        self.register_monitor(target, map, true, false)
    }

    /// Revokes a monitor. `true` means no notice for it will arrive; `false`
    /// means a notice has been queued already, or is being queued, and will
    /// arrive. An id this actor did not make, one that is unknown, and one
    /// that has fired and been dispatched all return `false`. A notice that
    /// was queued when a plain `demonitor` returned `false` is handled like
    /// any other: through `handle` when its turn comes, or, if the actor stops
    /// first, once at the stop with the notices the actor is owed.
    ///
    /// Kind: a revoke, answers `bool`. See [`signals`](crate::signals).
    ///
    /// Erlang/OTP: `erlang:demonitor(Ref, [info])`, which returns `false` when
    /// a `DOWN` has probably been placed in the queue already. The plain
    /// `demonitor/1` never returns `false`, and demonitoring a reference of
    /// another process is an error there; here it returns `false`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Down,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Watcher(tokio_actors::ActorHandle<Worker>);
    ///
    /// impl Actor for Watcher {
    ///     type Message = Option<Down>;
    ///     type Response = bool;
    ///     async fn handle(
    ///         &mut self,
    ///         _: Option<Down>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<bool> {
    ///         let id = ctx.monitor(&self.0, Some).expect("the watcher is running");
    ///         // The worker is still running, so the revoke is in time.
    ///         Ok(ctx.demonitor(id))
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let worker = Worker.spawn().await?;
    /// let watcher = Watcher(worker).spawn().await?;
    /// assert!(watcher.send(None).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn demonitor(&mut self, id: MonitorId) -> bool {
        if let Some(state) = self.monitors.as_deref_mut() {
            if state.contains(id) {
                return state.demonitor(id, false);
            }
        }
        self.monitors
            .as_deref_mut()
            .and_then(|state| state.groups.demonitor(id, false))
            .unwrap_or(false)
    }

    /// As [`demonitor`](Self::demonitor), and a notice that is queued but not
    /// yet handled is dropped, so it never reaches `handle`. `true` means the
    /// revoke was in time and no notice existed; `false` means there was no
    /// such monitor, or one had been queued and this call dropped it.
    ///
    /// Erlang/OTP: `erlang:demonitor(Ref, [flush, info])`, which removes a
    /// `DOWN` message from the queue and returns `false` when it had to.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, Down,
    /// };
    ///
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// struct Watcher(tokio_actors::ActorHandle<Worker>);
    ///
    /// impl Actor for Watcher {
    ///     type Message = Option<Down>;
    ///     type Response = bool;
    ///     async fn handle(
    ///         &mut self,
    ///         _: Option<Down>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<bool> {
    ///         let id = ctx.monitor(&self.0, Some).expect("the watcher is running");
    ///         // Nothing is queued, so the revoke is in time.
    ///         Ok(ctx.demonitor_flush(id))
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let worker = Worker.spawn().await?;
    /// let watcher = Watcher(worker).spawn().await?;
    /// assert!(watcher.send(None).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn demonitor_flush(&mut self, id: MonitorId) -> bool {
        if let Some(state) = self.monitors.as_deref_mut() {
            if state.contains(id) {
                return state.demonitor(id, true);
            }
        }
        self.monitors
            .as_deref_mut()
            .and_then(|state| state.groups.demonitor(id, true))
            .unwrap_or(false)
    }

    /// The tasks on the forwarder plane, ended ones the run loop has not yet
    /// taken included.
    #[cfg(test)]
    pub(crate) fn forwarder_tasks(&self) -> usize {
        self.ex().forwarders.len()
    }

    /// The monitor of a run known to have started (a spawn this actor has
    /// just made, or a top supervisor an application's start returned): as
    /// `monitor`, except that a run which has already ended
    /// gives its own reason, where `monitor` gives `NoProc`.
    pub(crate) fn monitor_spawned<T, F>(
        &mut self,
        target: T,
        map: F,
    ) -> Result<MonitorId, MonitorError>
    where
        T: IntoMonitorTarget,
        F: Fn(Down) -> A::Message + Send + Sync + 'static,
    {
        self.register_monitor(target, map, false, true)
    }

    /// The link of a spawn this actor has just made: as `link`, except that a
    /// run which has already ended delivers its own reason through the link
    /// rules, where `link` gives `NoProc`.
    pub(crate) fn link_spawned<B: Actor>(
        &mut self,
        target: &ActorHandle<B>,
    ) -> Result<(), LinkError> {
        link::link(self, target, false, true)
    }

    /// Makes a monitor and starts its task. `spawned` is true for the monitor
    /// of a run this actor has just spawned.
    fn register_monitor<T, F>(
        &mut self,
        target: T,
        map: F,
        priority: bool,
        spawned: bool,
    ) -> Result<MonitorId, MonitorError>
    where
        T: IntoMonitorTarget,
        F: Fn(Down) -> A::Message + Send + Sync + 'static,
    {
        if matches!(self.status(), ActorStatus::Stopping | ActorStatus::Stopped) {
            return Err(MonitorError::Stopping);
        }
        let resolved = monitor::resolve_target(&target, &self.resolved_system());
        let id = MonitorId::next();
        let map: Arc<MapFn<A::Message>> = Arc::new(map);
        let owner = self.owner();
        let (entry, task, token) = if spawned {
            monitor::new_spawned_entry::<A>(id, resolved, map, owner.clone())
        } else {
            monitor::new_entry::<A>(id, resolved, priority, map, owner.clone())
        };
        let handle = self.self_handle.clone();
        let state = self
            .monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)));
        state.bind(handle.stop_lane_ref().abort_record());
        state.insert(id, entry);
        let fut = monitor::monitor_forward(handle, task, token);
        self.spawn_forwarder(ForwarderKind::Monitor(id), fut, PanicSlot::default());
        Ok(id)
    }
}
