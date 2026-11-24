//! `spawn_child` and its builder.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::time;
use tokio::time::Instant;

use super::{release, ActorContext, ChildSpawnBuilder};
#[cfg(test)]
use crate::actor::context::BEFORE_CLOSE;
use crate::actor::handle::ActorHandle;
use crate::actor::incarnation::{StopLane, SubtreeLink};
use crate::actor::panic::{catch_sync, contain_waker_panic, payload_into_string};
use crate::actor::supervision::{
    await_predecessor, await_subtree, cancelled_before_factory, rules, run_attempt, slot_pair_for,
    Attempt, AutoShutdown, ChildLifecycle, ChildLinkGuard, ChildSpec, ChildState, Origin, OwnStop,
    Predecessor, RestartOutcome, SlotIo, TaskPlane, DEFAULT_KILL_GRACE,
};
#[cfg(feature = "sys")]
use crate::actor::sys::DebugOptions;
use crate::actor::sys::SysOptions;
use crate::actor::{runtime, Actor};
use crate::error::{ActorError, SpawnError, SupervisionError};
use crate::telemetry::{reports, Birth, PublishTo};
use crate::time_probe::{require_caller_timers, require_runtime_timers};
use crate::types::{ActorId, ChildInfo, RestartType, Shutdown, StopReason};

impl<A: Actor> ActorContext<A> {
    /// Creates a [`ChildSpawnBuilder`] for spawning a supervised child actor.
    ///
    /// The factory is called to create the initial instance, and stored for
    /// future restarts. Chain `.named()`, `.restart_type()`, `.shutdown()`,
    /// `.with_config()` before `.await`ing to customize.
    ///
    /// Unlike the top-level [`ActorExt::spawn`](crate::actor::ActorExt::spawn),
    /// this does not await the child's `pre_start`/`on_started` ack by
    /// default: the runner and restart budget already give the supervisor
    /// full, asynchronous visibility into a child's init failure, and
    /// synchronously awaiting that ack from inside the parent's own callback
    /// would invite the same self-call deadlock documented on
    /// [`ActorHandle::send`](crate::actor::handle::ActorHandle::send).
    ///
    /// By default, a child whose first `pre_start` or `on_started` fails ends
    /// with `StopReason::Failure` and is restarted under the budget like any
    /// crash (a `Temporary` child is removed), while the supervisor's own start
    /// succeeds, where an Erlang/OTP supervisor whose child fails to start
    /// stops the children already started and fails its own start. To start in
    /// order, spawn each child in `on_started` and await its `get_status()`,
    /// which the child answers only after its `pre_start` and `on_started`; a
    /// child that calls back into the supervisor and awaits the answer during
    /// its initialization then waits forever, as an Erlang/OTP child does when
    /// its call to the supervisor has an infinite timeout, and a
    /// `start_timeout` on the supervisor's own spawn ends the start with
    /// [`SpawnError::StartTimeout`]. With `?` on each `get_status()` (each
    /// error mapped with `ActorError::user(e.to_string())`), the supervisor's
    /// `on_started` returns the error, its awaited spawn returns
    /// `SpawnError::Init`, and its teardown stops the children already started.
    /// [`with_strict_start`](crate::actor::supervision::SupervisionConfig::with_strict_start)
    /// does the same wait for every child spawned while the supervisor starts,
    /// and [`await_init`](ChildSpawnBuilder::await_init) awaits one child's
    /// initialization.
    ///
    /// Because this start is not awaited, a `Kill` of the child that lands
    /// before its task first runs ends that incarnation before any start hook
    /// begins, and the child then follows its restart type like after any
    /// other `Kill`.
    ///
    /// The spawn fails with [`SpawnError::SupervisorStopping`] when called
    /// inside [`on_child_stopped`](crate::actor::Actor::on_child_stopped)
    /// while the supervisor itself is stopping.
    ///
    /// A restart of a child begins only after the child's previous incarnation
    /// and every actor it spawned have ended, waiting without blocking the
    /// supervisor. A spawn under the id of a child that was deleted or removed
    /// waits likewise, inside the calling handler, until every actor the
    /// previous child spawned has ended, and fails with
    /// [`SpawnError::ChildUnresponsive`] if one of them never yields. Each wait
    /// is bounded by the longest chain of stop budgets in the subtree (a
    /// worker's kill grace, a supervisor child's `Shutdown` window plus its
    /// kill grace, and for a worker that has called `trap_exit` its own
    /// `Shutdown` window as well), each followed by a short settle time, plus
    /// one more settle time, and unbounded only while a supervisor child under
    /// [`Shutdown::Infinity`] runs in it, or a worker under `Infinity` that
    /// traps exit signals, or a budget is too large to represent. Past that
    /// bound, the wait gives no verdict on an actor before the abort the system
    /// owes it has had the settle time to end it: an abort that is late holds
    /// the wait, for at most the bound once more plus one more settle time.
    ///
    /// A child spawned while a group restart is pending starts at once; it is
    /// not part of that restart, though a retry after a member's restart fails
    /// includes the new child: a `OneForAll` retry stops every child and
    /// restarts each one that is not `Temporary` and was not released by
    /// `terminate_child`, and a `RestForOne` retry does the same from the
    /// child that failed onward.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult, RestartType, SupervisionConfig};
    ///
    /// #[derive(Default)]
    /// struct Supervisor;
    /// #[derive(Default)]
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    /// }
    ///
    /// impl Actor for Supervisor {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         // Defaults: anonymous, Permanent, Shutdown::Timeout(5s)
    ///         ctx.spawn_child(Worker::default).await?;
    ///
    ///         // Named + transient
    ///         ctx.spawn_child(Worker::default)
    ///             .named("worker")
    ///             .restart_type(RestartType::Transient)
    ///             .await?;
    ///         Ok(())
    ///     }
    /// }
    /// ```
    ///
    /// # Errors
    /// - [`SpawnError::NotASupervisor`]
    ///   if this actor has no supervision config.
    /// - [`SpawnError::DuplicateChild`]
    ///   if the supervisor holds a child spec under this name, whatever the
    ///   child is doing: it runs (one whose death the supervisor has not
    ///   handled yet included), it is being restarted, or it is stopped and
    ///   its spec is kept until [`delete_child`](Self::delete_child) removes
    ///   it ([`terminate_child`](Self::terminate_child) says which stopped
    ///   children keep their spec). The factory is never called. A call made
    ///   inside [`on_child_stopped`](crate::actor::Actor::on_child_stopped)
    ///   while the supervisor itself is stopping is answered
    ///   [`SpawnError::SupervisorStopping`] first. Erlang/OTP answers
    ///   `{already_started, Pid}` for a running child, or one whose exit the
    ///   supervisor has not processed yet, and `already_present` for the
    ///   others, a child being restarted included; this call answers
    ///   `DuplicateChild` for all of them, and [`children`](Self::children)
    ///   lists the specs the supervisor holds.
    /// - [`SpawnError::ChildUnresponsive`]
    ///   if an actor of a deleted or removed child under this id, or one that
    ///   child spawned, did not end within the bound of that subtree.
    /// - [`SpawnError::InvalidSupervision`]
    ///   if the child is significant under [`AutoShutdown::Never`] or with
    ///   restart type `Permanent`, or its own supervision sets an
    ///   [`AutoShutdown`] mode under `SimpleOneForOne`. The factory is never
    ///   called.
    /// - other [`SpawnError`] variants if the child
    ///   fails to spawn.
    pub fn spawn_child<F, C>(&mut self, factory: F) -> ChildSpawnBuilder<'_, A, F, C>
    where
        F: Fn() -> C + Send + Sync + 'static,
        C: Actor,
    {
        ChildSpawnBuilder {
            ctx: self,
            factory,
            name: None,
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::default(),
            config: None,
            start_timeout: None,
            kill_grace: DEFAULT_KILL_GRACE,
            runtime: None,
            significant: false,
            await_init: false,
            sys: SysOptions::default(),
            _child: std::marker::PhantomData,
        }
    }

    /// The refusals of a spawn that need no wait and change nothing, in the
    /// order every spawn answers them: not a supervisor, the supervisor is
    /// stopping, the name is taken. An unnamed spawn has no name to take.
    fn refuse_spawn(&self, id: Option<&ActorId>) -> Result<(), SpawnError> {
        let Some(sup) = self.supervision.as_deref() else {
            return Err(SpawnError::NotASupervisor);
        };
        if sup.final_delivery {
            return Err(SpawnError::SupervisorStopping);
        }
        match id {
            Some(id) if sup.registry.get(id).is_some() => {
                Err(SpawnError::DuplicateChild(id.clone()))
            }
            _ => Ok(()),
        }
    }

    /// The refusals of a spawn that come from combining options Erlang/OTP
    /// refuses, answered before any factory runs: a significant child under
    /// `AutoShutdown::Never`, a significant `Permanent` child, and a child
    /// that is itself a supervisor with `auto_shutdown` set under
    /// `SimpleOneForOne`. A call that another refusal answers first (not a
    /// supervisor, stopping) passes here and meets that refusal next. The
    /// refusal of the child's own configuration raises the spawn-refused
    /// event, as the same refusal of a root spawn does; the other two come
    /// from options with no root counterpart and raise none.
    fn refuse_setup(
        &self,
        name: Option<&str>,
        significant: bool,
        restart_type: RestartType,
        child_config: Option<&runtime::ActorConfig>,
    ) -> Result<(), SpawnError> {
        let Some(sup) = self.supervision.as_deref() else {
            return Ok(());
        };
        if sup.final_delivery {
            return Ok(());
        }
        if let Some(config) = child_config {
            runtime::supervision_setup_error(config)
                .map_err(|err| runtime::refused(self.system.as_ref(), name, err))?;
        }
        if significant {
            if sup.config.auto_shutdown == AutoShutdown::Never {
                return Err(SpawnError::InvalidSupervision(
                    "significant child under auto_shutdown never",
                ));
            }
            if restart_type == RestartType::Permanent {
                return Err(SpawnError::InvalidSupervision(
                    "significant child with restart permanent",
                ));
            }
        }
        Ok(())
    }

    /// The wait of a spawn under the id of a removed child: until every actor
    /// the removed child's last incarnation spawned has ended. The refusals
    /// that need no wait are answered first and in their own order, so a
    /// refused call never reads, prunes or waits on the removed children; a
    /// spawn under an id nobody removed completes without waiting.
    async fn await_retired_subtree(&mut self, name: Option<&Arc<str>>) -> Result<(), SpawnError> {
        let Some(name) = name else {
            return Ok(());
        };
        let id = ActorId::from_shared(Arc::clone(name));
        self.refuse_spawn(Some(&id))?;
        let Some(sup) = self.supervision.as_deref_mut() else {
            return Err(SpawnError::NotASupervisor);
        };
        let Some(subtree) = sup.retired_subtree(&id) else {
            return Ok(());
        };
        match await_subtree(&subtree).await {
            rules::WaitAction::Escalate => Err(SpawnError::ChildUnresponsive(id)),
            _ => Ok(()),
        }
    }

    /// Internal spawn logic used by [`ChildSpawnBuilder`].
    #[allow(clippy::too_many_arguments)]
    fn spawn_child_internal<F, C>(
        &mut self,
        factory: F,
        name: Option<Arc<str>>,
        restart_type: RestartType,
        shutdown: Shutdown,
        config: Option<runtime::ActorConfig>,
        start_timeout: Option<Duration>,
        kill_grace: Duration,
        placement: Option<Handle>,
        flags: ChildFlags,
        sys: SysOptions,
    ) -> Result<(ActorHandle<C>, Option<InitWait>), SpawnError>
    where
        F: Fn() -> C + Send + Sync + 'static,
        C: Actor,
    {
        // A spec the supervisor holds under this id, whatever its child is
        // doing (running, being restarted, or stopped with the spec kept),
        // rejects the spawn before the factory ever runs: OTP answers
        // `already_started` or `already_present` there. `delete_child` clears
        // the spec so the id can be reused.
        let named_id = name
            .as_ref()
            .map(|name| ActorId::from_shared(Arc::clone(name)));
        let sensitive = sys.effective_sensitive::<C>();
        let child_id: ActorId = match named_id.as_ref() {
            Some(named) => named.clone(),
            None => uuid::Uuid::new_v4().to_string().into(),
        };
        if let Err(err) = self.refuse_spawn(named_id.as_ref()) {
            report_refused(&self.actor_id, named_id.as_ref(), &err, sensitive);
            return Err(err);
        }

        let actor = factory();
        let child_config = config.unwrap_or_default();
        let is_supervisor = child_config.supervision.is_some();
        // What the child adds to the budget of the subtree it hangs from,
        // and whether its orderly stop has no bound: both are properties of
        // the spec, so every incarnation of the child attaches the same way.
        let subtree_own = rules::own_fin(is_supervisor, shutdown, kill_grace);
        // What bounds the child's stop once its supervisor is gone, and what
        // its link guard does to it then; both are properties of the spec
        // too, so every incarnation is stopped the same way.
        let own_stop = OwnStop {
            is_supervisor,
            shutdown,
            kill_grace,
        };
        let inherited = rules::inherited_stop(is_supervisor, shutdown, kill_grace);
        let subtree_infinity = is_supervisor && shutdown == Shutdown::Infinity;
        let subtree_trap_extra = rules::trap_extra(is_supervisor, shutdown);
        let subtree_trap_infinity = rules::trap_infinity(is_supervisor, shutdown);
        let subtree_parent = self.subtree.token();

        // The runtime every incarnation of this child runs on, its link guard
        // and its reaper too: the one the builder placed it on, else the
        // supervisor's. Resolved once, so a restart lands where the first
        // incarnation did.
        let guard_runtime = placement.unwrap_or_else(|| self.runtime_handle().clone());
        // The first start of a child is acknowledged to nobody, except when
        // the call awaits it or the supervisor's strict start waits for it:
        // then the child's acknowledgment goes to a receiver kept for that.
        let strict = !flags.await_init && self.supervision.as_deref().is_some_and(|s| s.init_phase);
        let (ack_tx, mut ack_rx) = if flags.await_init || strict {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        // Every registration - this initial spawn included - draws its
        // incarnation token from the system's single monotonic counter, so
        // no two incarnations of any child, under any name, ever collide
        // (see `ActorSystem::next_incarnation`).
        let incarnation = self.resolved_system().next_incarnation();
        let (child_handle, join_handle, child_subtree) = runtime::spawn_actor_in(
            child_id.clone(),
            actor,
            child_config.clone(),
            name.clone(),
            self.system.clone(),
            ack_tx,
            false,
            kill_grace,
            Some(SubtreeLink {
                parent: subtree_parent.clone(),
                own: subtree_own,
                infinity: subtree_infinity,
                trap_extra: subtree_trap_extra,
                trap_infinity: subtree_trap_infinity,
            }),
            Some(&guard_runtime),
            Birth {
                incarnation: Some(incarnation),
                parent: Some((
                    &self.actor_id,
                    self.stop_lane().abort_record().incarnation().number(),
                )),
                publish: None,
            },
            sys,
        )
        .inspect_err(|err| {
            reports::start_error(
                &self.actor_id,
                &child_id,
                Some(incarnation),
                || StopReason::Failure(ActorError::Spawn(err.clone())),
                sensitive,
            );
        })?;

        // The parent spawns and owns the runner of the initial incarnation,
        // together with its link guard - constructed here, in the parent's
        // own synchronous code, before the runner task is spawned, so an
        // abort landing before the runner is ever polled still fires it.
        // The abort handle is taken first - the Kill escalation backstop.
        let abort = join_handle.abort_handle();
        let resolved_system = self.resolved_system();
        let reaper = resolved_system.reaper_handle(&guard_runtime);
        // Cloned before the initial guard consumes `reaper` (and the system)
        // by value: the restart closure below needs the same per-system
        // reaper feed and system handle for every future incarnation's own
        // link guard.
        let restart_reaper = reaper.clone();
        let restart_system = resolved_system.clone();
        let guard = ChildLinkGuard::new(
            child_handle.stop_lane(),
            abort.clone(),
            reaper,
            inherited,
            resolved_system,
            guard_runtime.clone(),
        );
        let verdicts = self
            .supervision
            .as_ref()
            .expect("supervision presence checked above")
            .verdicts
            .clone();
        let (ctl, io) = slot_pair_for(verdicts, Some((child_id.clone(), incarnation)));
        io.publish(
            child_handle.stop_lane(),
            child_handle.status_rx(),
            own_stop,
            Some(child_subtree.clone()),
        );

        let system_clone = self.system.clone();

        let sup = self
            .supervision
            .as_mut()
            .expect("supervision presence checked above");
        // Named before the first runner exists, so every runner lands its
        // death on the supervisor's record.
        sup.registry
            .adopt_owner(self.self_handle.stop_lane_ref().abort_record());
        let owner = sup.registry.owner_record();
        runtime::claim_death(
            owner.as_ref(),
            &child_id,
            incarnation,
            child_handle.stop_lane_ref().abort_record(),
        );
        let task = runtime::spawn_runner(
            &mut sup.death_set,
            child_id.clone(),
            incarnation,
            join_handle,
            None,
            child_handle.stop_lane(),
            io,
            guard,
            kill_grace,
            owner,
        );
        sup.task_owner
            .insert(task.id(), (child_id.clone(), incarnation, TaskPlane::Death));

        let predecessor = ctl.predecessor(child_subtree);
        let child_state = ChildState {
            id: child_id.clone(),
            name: name.clone(),
            spec: ChildSpec {
                restart_type,
                shutdown,
                start_timeout,
                kill_grace,
                is_supervisor,
                significant: flags.significant,
                // The spec records whether the child is sensitive by its
                // type or by the spawn, which the supervisor's reports read.
                sys: SysOptions { sensitive, ..sys },
            },
            ctl,
            lifecycle: ChildLifecycle::Running(incarnation),
            origin: Origin::Initial,
            predecessor: Some(predecessor),
        };
        let wait = if flags.await_init {
            ack_rx.take().map(|ack| InitWait {
                ack,
                child_id: child_id.clone(),
                incarnation,
                lane: child_handle.stop_lane(),
                abort: abort.clone(),
            })
        } else {
            None
        };

        // The supervisor's id, which an attempt's log lines name, and its
        // incarnation number, which every restarted incarnation's registry
        // entry names with the id.
        let restart_supervisor = self.actor_id().clone();
        let restart_supervisor_seq = self.stop_lane().abort_record().incarnation().number();
        let sup = self
            .supervision
            .as_mut()
            .expect("supervision presence checked above");
        sup.registry
            .adopt_owner(self.self_handle.stop_lane_ref().abort_record());
        sup.registry.register(child_state);
        if strict {
            if let Some(ack) = ack_rx {
                sup.push_strict_ack(child_id.clone(), incarnation, ack);
            }
        }

        // Restart closure: captures the child's spec BY VALUE (original id,
        // name, resolved config) so every restart reuses it exactly - the
        // Rust equivalent of OTP child-spec immutability. The child keeps its
        // ActorId across restarts, named or anonymous.
        let factory = Arc::new(factory);
        let restart_id = child_id.clone();
        let restart_name = name;
        let restart_config = child_config;
        // Captured alongside the rest of the restart spec so the restart
        // closure (whose only input is the incarnation sequence number) can
        // bound its wait for this child's init without reading the registry.
        let restart_start_timeout = start_timeout;
        // The kill grace every fresh incarnation is spawned, guarded and
        // laddered with, exactly like the initial spawn above.
        let restart_kill_grace = kill_grace;
        // The `sys` options every fresh incarnation is spawned with, as the
        // initial spawn was.
        let restart_sys = sys;
        let restart_sensitive = sensitive;
        let restart_runtime = guard_runtime;

        reports::progress(
            &self.actor_id,
            &child_id,
            incarnation,
            reports::Progress::Started,
        );
        sup.restart_fns.insert(
            child_id,
            Box::new(
                move |seq, mut io: SlotIo, predecessor: Option<Predecessor>| {
                    // The attempt holds the factory weakly: the spec's box
                    // holds its strong reference, so its drop runs where the
                    // spec is removed, unless the spec goes while the attempt
                    // builds; see `build_for_attempt`.
                    let factory = Arc::downgrade(&factory);
                    let supervisor = restart_supervisor.clone();
                    let system = system_clone.clone();
                    let child_id = restart_id.clone();
                    let name = restart_name.clone();
                    let config = restart_config.clone();
                    let start_timeout = restart_start_timeout;
                    let kill_grace = restart_kill_grace;
                    let sys = restart_sys;
                    let runtime = restart_runtime.clone();
                    let reaper = restart_reaper.clone();
                    let guard_system = restart_system.clone();
                    let subtree_parent = subtree_parent.clone();
                    Box::pin(async move {
                        // The previous incarnation's whole subtree ends before
                        // anything is built: the wait runs here, in the restart
                        // plane, never in the supervisor's loop. A stop order or
                        // the group's cancel ends it, and a subtree that outlives
                        // its bound is reported, never started beside.
                        match await_predecessor(predecessor.as_ref(), &mut io).await {
                            rules::WaitAction::Cancel => {
                                return RestartOutcome::Cancelled {
                                    child_id,
                                    incarnation: seq,
                                    drop_panic: None,
                                };
                            }
                            rules::WaitAction::Escalate => {
                                return RestartOutcome::PredecessorUnresponsive {
                                    child_id,
                                    incarnation: seq,
                                };
                            }
                            rules::WaitAction::Proceed | rules::WaitAction::Keep => {}
                        }

                        // A stop order or the group's cancel written before the
                        // factory runs ends the attempt: nothing exists to stop,
                        // and nothing is built.
                        if cancelled_before_factory(&mut io) {
                            return RestartOutcome::Cancelled {
                                child_id,
                                incarnation: seq,
                                drop_panic: None,
                            };
                        }

                        let actor = match build_for_attempt_of(
                            &factory,
                            &io,
                            &supervisor,
                            &child_id,
                            seq,
                            restart_sensitive,
                        ) {
                            Ok(actor) => actor,
                            Err(outcome) => return *outcome,
                        };

                        let (ack_tx, ack_rx) = oneshot::channel();
                        let (handle, join, _) = match runtime::spawn_actor_in(
                            child_id.clone(),
                            actor,
                            config,
                            name,
                            system,
                            Some(ack_tx),
                            false,
                            kill_grace,
                            Some(SubtreeLink {
                                parent: subtree_parent,
                                own: subtree_own,
                                infinity: subtree_infinity,
                                trap_extra: subtree_trap_extra,
                                trap_infinity: subtree_trap_infinity,
                            }),
                            Some(&runtime),
                            Birth {
                                incarnation: Some(seq),
                                parent: Some((&supervisor, restart_supervisor_seq)),
                                publish: Some(PublishTo {
                                    io: &io,
                                    own: &own_stop,
                                }),
                            },
                            sys,
                        ) {
                            Ok(spawned) => spawned,
                            Err(err) => {
                                let reason = StopReason::Failure(ActorError::Spawn(err));
                                return RestartOutcome::Failed {
                                    child_id,
                                    incarnation: seq,
                                    reason,
                                };
                            }
                        };

                        // The spawn published the incarnation to the slot before
                        // its task could run. A test holds the attempt here, so
                        // the incarnation can run to its end before the attempt
                        // goes on.
                        #[cfg(test)]
                        runtime::end_hooks::wait_hold(runtime::end_hooks::SPAWNED, &child_id).await;

                        // Armed the instant the spawn succeeded - before the init
                        // ack is ever awaited - so a parent that dies mid-restart
                        // (dropping its whole restart plane) still kills this
                        // fresh incarnation through the guard's own `Drop`,
                        // whatever stage the wait below is at: pre-ack, mid-init,
                        // or hung forever under `start_timeout: None` (the guard,
                        // not the ack, is the backstop there). The abort handle
                        // is taken first - the Kill escalation backstop.
                        let abort = join.abort_handle();
                        let new_stop_lane = handle.stop_lane();
                        let guard = ChildLinkGuard::new(
                            new_stop_lane.clone(),
                            abort.clone(),
                            reaper,
                            inherited,
                            guard_system,
                            runtime,
                        );

                        // From here the attempt's executor owns the incarnation:
                        // it awaits the same init contract the top-level spawn
                        // awaits (pre_start + on_started), bounded by the child's
                        // start_timeout if one was set (None waits indefinitely,
                        // OTP start_link parity), and honors a stop order at
                        // every moment of that wait.
                        run_attempt(Attempt {
                            child_id,
                            incarnation: seq,
                            join,
                            lane: new_stop_lane,
                            guard,
                            ack: ack_rx,
                            start_timeout,
                            grace: kill_grace,
                            io,
                        })
                        .await
                    })
                },
            ),
        );

        Ok((child_handle, wait))
    }

    /// The wait of a spawn with `await_init`, after the child is registered:
    /// the child's initialization, bounded by `start_timeout` when one is set.
    /// A child that did not start is discarded: its start is cut when the bound
    /// passed, its spec is removed and its death report is silenced, so the
    /// supervisor neither calls `on_child_stopped` nor charges its budget.
    async fn finish_await_init<C: Actor>(
        &mut self,
        handle: ActorHandle<C>,
        wait: InitWait,
        start_timeout: Option<Duration>,
    ) -> Result<ActorHandle<C>, SpawnError> {
        let InitWait {
            mut ack,
            child_id,
            incarnation,
            lane,
            abort,
        } = wait;
        let end = match start_timeout {
            None => init_end((&mut ack).await),
            Some(bound) => {
                let deadline = runtime::saturating_deadline(Instant::now(), bound);
                bounded_ack(&mut ack, time::sleep_until(deadline), || {
                    crate::actor::start_bound::cut_start(&lane, &abort);
                })
                .await
            }
        };
        let error = match end {
            InitEnd::Started => return Ok(handle),
            InitEnd::Failed(err) => SpawnError::Init(Box::new(err)),
            InitEnd::TimedOut => SpawnError::StartTimeout,
            InitEnd::Closed => {
                let mut status = handle.status_rx();
                match crate::system::wait_final_reason(&mut status).await {
                    StopReason::Failure(err) => SpawnError::Init(Box::new(err)),
                    _ => SpawnError::Killed,
                }
            }
        };
        // The child keeps no spec and owes no report: its end is dropped when
        // it arrives. A start under the same id waits for the failed
        // incarnation's subtree, which the removal keeps under the id.
        let mut factory = None;
        if let Some(sup) = self.supervision_mut() {
            sup.silenced.push((child_id.clone(), incarnation));
            factory = sup.remove_spec(&child_id);
        }
        // The factory is user code: dropped as the call's last step, a panic
        // in its drop is a panic of the callback that called it.
        drop(factory);
        Err(error)
    }

    /// Returns introspection info for all supervised children. To count the
    /// children (all of them, and the live ones through `is_alive`, as
    /// Erlang/OTP's `supervisor:count_children` does for specs and active) or
    /// to find one by id, as a scan of `supervisor:which_children` does, use
    /// this list; its entries also carry the name, restart type and `Shutdown`,
    /// and whether the child is itself a supervisor (`is_supervisor`).
    ///
    /// A child is alive while it has a started incarnation whose stop, if
    /// any, has not completed, and its restart is pending while an attempt is
    /// in flight, while its death awaits its restart decision, and while a
    /// pending group restart will restart it.
    pub fn children(&self) -> Vec<ChildInfo> {
        self.supervision
            .as_deref()
            .map_or(Vec::new(), |s| s.children_info())
    }
}

/// The `Debug` view of a child's start bound: `unbounded` for the default
/// and for `unbounded_start`, the duration for `start_timeout`.
struct StartBound(Option<Duration>);

impl std::fmt::Debug for StartBound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            None => f.write_str("unbounded"),
            Some(dur) => std::fmt::Debug::fmt(&dur, f),
        }
    }
}

impl<'ctx, A: Actor, F, C: Actor> std::fmt::Debug for ChildSpawnBuilder<'ctx, A, F, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSpawnBuilder")
            .field("name", &self.name)
            .field("restart_type", &self.restart_type)
            .field("shutdown", &self.shutdown)
            .field("start_timeout", &StartBound(self.start_timeout))
            .field("kill_grace", &self.kill_grace)
            .field("significant", &self.significant)
            .field("await_init", &self.await_init)
            .finish_non_exhaustive()
    }
}

impl<'ctx, A: Actor, F, C: Actor> ChildSpawnBuilder<'ctx, A, F, C>
where
    F: Fn() -> C + Send + Sync + 'static,
{
    /// Assigns a name to the child. The name also serves as the child's
    /// [`ActorId`].
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the child's [`RestartType`]. Default: [`RestartType::Permanent`].
    pub fn restart_type(mut self, restart_type: RestartType) -> Self {
        self.restart_type = restart_type;
        self
    }

    /// Sets the child's [`Shutdown`] policy. Default: [`Shutdown::Timeout(5s)`](Shutdown::Timeout).
    ///
    /// When the child is itself a supervisor, its `Shutdown` must cover its
    /// own children: at least the sum of their stop budgets (each child's
    /// `Shutdown`, plus its kill grace, plus the short wait that confirms an
    /// abort), or the largest of them for a `SimpleOneForOne` pool. A shorter
    /// window escalates the child to `Kill` and its remaining children take
    /// the killed-supervisor path. Erlang/OTP asks for `infinity` here: "If
    /// the child process is another supervisor, the shutdown time must be set
    /// to infinity to give the subtree ample time to shut down."
    pub fn shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Sets how long this child is given, after a `Kill` is raised on it, to
    /// reach its next turn boundary before its task is aborted. Default:
    /// 120 ms. Kept across every restart of the child.
    ///
    /// `Duration::ZERO` aborts the task as soon as the `Kill` is raised, so a
    /// callback in progress is cut at its next `.await` and never observes
    /// the `Kill` itself. The grace applies to every kill of this child:
    /// [`ActorSystem::kill`](crate::system::ActorSystem::kill),
    /// [`ActorSystem::kill_by_id`](crate::system::ActorSystem::kill_by_id),
    /// [`ActorHandle::stop`](crate::actor::handle::ActorHandle::stop) with
    /// [`StopReason::Kill`], the kill step of
    /// [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within)
    /// and of system shutdown, its supervisor's stop of it, and the kill a
    /// worker that does not trap exit signals receives when its own supervisor
    /// is killed. An actor that is itself a supervisor, or a worker that traps
    /// exit signals, stops in order when its supervisor is killed,
    /// bounded by its own [`Shutdown`], and the
    /// grace applies to the `Kill` raised once that window has passed;
    /// [`ActorHandle::stop_within`](crate::actor::handle::ActorHandle::stop_within)
    /// called with `StopReason::Kill` waits its own `within` instead, for
    /// that call. A grace beyond 30 years is capped there, so in practice it
    /// never aborts. A callback that never yields survives the abort as well
    /// and is reported unresponsive.
    pub fn kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = grace;
        self
    }

    /// Starts the child with the given debug options on: statistics, the log
    /// and the trace, as the [`sys`](crate::actor::sys) calls would turn them
    /// on. Every restart of the child starts from the same options, not from
    /// the toggles of the incarnation before.
    ///
    /// Needs the `sys` feature, which is on by default.
    #[cfg(feature = "sys")]
    pub fn debug(mut self, options: DebugOptions) -> Self {
        self.sys.debug = options;
        self
    }

    /// Lets [`inspect`](crate::actor::sys::Sys::inspect) and
    /// [`replace_state`](crate::actor::sys::Sys::replace_state) reach the
    /// child's value, in every incarnation. A sensitive child stays closed.
    ///
    /// Needs the `sys` feature, which is on by default.
    #[cfg(feature = "sys")]
    pub fn allow_state_access(mut self) -> Self {
        self.sys.allow_state_access = true;
        self
    }

    /// Makes the child sensitive, as [`Actor::SENSITIVE`] does for every
    /// spawn of its type.
    pub fn sensitive(mut self) -> Self {
        self.sys.sensitive = true;
        self
    }

    /// Runs the child on the Tokio runtime behind `handle` instead of the
    /// supervisor's. Every restart of the child runs there too, and so does
    /// everything the child spawns in turn: its own children, its timers and
    /// its streams. The supervisor keeps the stop ladder of a child it stops
    /// or restarts: the child's [`Shutdown`] window, the kill grace and the
    /// settle that follow it, and the restart decisions run on the
    /// supervisor's runtime. A `Kill` raised from outside that ladder
    /// (`ActorSystem::kill`, `ActorSystem::kill_by_id`, `stop` with
    /// `StopReason::Kill` on the child's handle, or the `Kill` step of
    /// `stop_within`), and the end a gone supervisor leaves a child (if the
    /// child is a supervisor, its `Shutdown` window, then its kill grace), wait
    /// on the child's runtime: the kill grace, or the `within` of a
    /// `stop_within` called with `StopReason::Kill`.
    ///
    /// The runtime behind `handle` needs the time driver
    /// (`Builder::enable_time` or `enable_all`), because the abort timers that
    /// run on it (the kill grace of a `Kill` from outside the supervisor's
    /// ladder, and the end a gone supervisor leaves the child) sleep on it.
    /// Awaiting the builder panics, before the factory
    /// runs and
    /// before anything is registered, when the runtime was built without it:
    /// a placed child would lose its kill grace without a sign. The refusals
    /// of [`spawn_child`](ActorContext::spawn_child)
    /// ([`SpawnError::NotASupervisor`], [`SpawnError::SupervisorStopping`]
    /// and [`SpawnError::DuplicateChild`]) and
    /// [`SpawnError::InvalidSupervision`] are answered first, so a spawn
    /// refused for one of those four reasons never panics; any other
    /// refusal, such as a zero child mailbox capacity or a name already taken
    /// in the system, is answered after the check and panics first on a
    /// runtime without the driver. The check asks the runtime whether it
    /// has been shut down before it asks about the driver, so a runtime that
    /// has ended passes it (see below) and no panic hook runs for it. With
    /// `panic = "abort"` there is no probe and so no check at the entry: the
    /// first timer the crate starts on a runtime without the driver ends the
    /// process.
    ///
    /// When the placed runtime is not running the child:
    ///
    /// - **Gone** (the `Runtime` was dropped or shut down): the child is
    ///   killed. [`spawn_child`](ActorContext::spawn_child) still returns
    ///   `Ok`, each restart fails at once, and each failure charges the
    ///   restart budget until it is spent.
    /// - **Alive but undriven** (a current-thread runtime that nothing is
    ///   running): no incarnation starts. A child spawned onto it is never
    ///   confirmed stopped: under a bounded `Shutdown`,
    ///   [`stop_child`](ActorContext::stop_child) and
    ///   [`terminate_child`](ActorContext::terminate_child) answer
    ///   [`SupervisionError::ChildUnresponsive`] for it once its `Shutdown`
    ///   window, kill grace and settle have passed. A restart onto such a
    ///   runtime stays pending: the crash that began it is charged once and the
    ///   wait charges nothing more; [`stop_child`](ActorContext::stop_child)
    ///   and [`terminate_child`](ActorContext::terminate_child) answer
    ///   [`SupervisionError::ChildRestarting`] for it until the runtime is
    ///   driven again or the supervisor stops. Under `OneForAll` and
    ///   `RestForOne`, with a [`start_timeout`](Self::start_timeout) on the
    ///   child, a group restart that waits for it cannot finish: once the
    ///   start timeout, the kill grace and the settle have passed, the
    ///   supervisor stops with
    ///   [`SupervisionError::ChildUnresponsive`] for that child.
    pub fn on_runtime(mut self, handle: &Handle) -> Self {
        self.runtime = Some(handle.clone());
        self
    }

    /// Overrides the default [`ActorConfig`](runtime::ActorConfig) for the child.
    pub fn with_config(mut self, config: runtime::ActorConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Bounds the wait for the initialization of each restart of this child.
    /// By default a restart's initialization waits without a bound, under any
    /// [`Shutdown`], as an Erlang/OTP supervisor's does; a bound is a
    /// deliberate departure that trades strict parity for a supervisor that
    /// one child's hung initialization cannot stall. A restart that does not
    /// finish `pre_start` and `on_started` within `dur` is cut: `Kill` is
    /// raised on it and its task is aborted once its kill grace has passed.
    /// A plain [`spawn_child`](ActorContext::spawn_child) never awaits the
    /// first start, so for it this bound applies to restarts only; with
    /// [`await_init`](Self::await_init) the bound also covers the first start
    /// and the awaited call returns [`SpawnError::StartTimeout`]. The strict
    /// start of a supervisor
    /// ([`with_strict_start`](crate::SupervisionConfig::with_strict_start))
    /// waits for the first start of each child outside it. A bound beyond 30
    /// years is treated as 30 years. The last
    /// call between this method and [`unbounded_start`](Self::unbounded_start)
    /// wins.
    pub fn start_timeout(mut self, dur: Duration) -> Self {
        self.start_timeout = Some(dur);
        self
    }

    /// Writes the default out: the initialization of each restart of this
    /// child waits without a bound, as an Erlang/OTP supervisor's does, under
    /// any [`Shutdown`]. It undoes an earlier
    /// [`start_timeout`](Self::start_timeout), and the last call between the
    /// two wins.
    ///
    /// This differs from `start_timeout(Duration::MAX)`: that call is a bound
    /// of 30 years, which arms a timer that a paused Tokio clock can fire
    /// once it advances past it, and its [`Debug`](std::fmt::Debug) output
    /// shows the duration. Here no timer is armed and the
    /// [`Debug`](std::fmt::Debug) output reads `unbounded`.
    pub fn unbounded_start(mut self) -> Self {
        self.start_timeout = None;
        self
    }

    /// Marks the child as significant, as Erlang/OTP's `significant` child
    /// flag does: when the supervisor does not restart the child, its end can
    /// shut the supervisor down, under the supervisor's
    /// [`AutoShutdown`] mode.
    /// Default: `false`.
    ///
    /// Only an end the supervisor did not order counts: a `Transient` child's
    /// normal exit or a `Temporary` child's exit of any kind. A child stopped
    /// by [`stop_child`](ActorContext::stop_child),
    /// [`terminate_child`](ActorContext::terminate_child) or a group restart
    /// never shuts the supervisor down.
    ///
    /// Awaiting the builder answers
    /// [`SpawnError::InvalidSupervision`] before any factory runs when
    /// `significant` is `true` and the supervisor's mode is
    /// [`AutoShutdown::Never`],
    /// or the child is [`RestartType::Permanent`], which Erlang/OTP refuses
    /// too.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorResult, AutoShutdown, RestartType, SupervisionConfig,
    /// };
    ///
    /// #[derive(Default)]
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    /// }
    ///
    /// struct Boss;
    ///
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.spawn_child(Worker::default)
    ///             .restart_type(RestartType::Transient)
    ///             .significant(true)
    ///             .await?;
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let _boss = Boss
    ///     .spawn()
    ///     .with_supervision(
    ///         SupervisionConfig::one_for_one().with_auto_shutdown(AutoShutdown::AnySignificant),
    ///     )
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn significant(mut self, significant: bool) -> Self {
        self.significant = significant;
        self
    }

    /// Makes the awaited builder return after the child's initialization
    /// (`pre_start` and `on_started`), as Erlang/OTP's `supervisor:start_child`
    /// does, instead of once the child is registered. Default: not awaited.
    ///
    /// On success the call returns the child's handle with the child running.
    /// If the initialization returns an error or panics, the call returns
    /// [`SpawnError::Init`] with it and the child is discarded: no spec is
    /// kept, [`on_child_stopped`](crate::actor::Actor::on_child_stopped) is not
    /// called for it and the restart budget is not charged. A child that was
    /// killed first gives [`SpawnError::Killed`]. A spawn under the same id
    /// afterwards waits for the failed incarnation's subtree to end, as after
    /// [`delete_child`](ActorContext::delete_child).
    ///
    /// The wait has no bound by default, as Erlang/OTP's start has none;
    /// [`start_timeout`](Self::start_timeout) bounds it, and a child whose
    /// initialization is still running at the bound is cut (`Kill` raised,
    /// task aborted) and the call returns [`SpawnError::StartTimeout`]. With
    /// that bound the runtime needs the time driver, and awaiting the builder
    /// panics without it. A child that
    /// calls its supervisor during its own initialization waits for the
    /// supervisor, which waits for it, until that bound, as in Erlang/OTP.
    ///
    /// Dropping the future before it completes leaves the child registered
    /// as a plain [`spawn_child`](ActorContext::spawn_child) leaves it; a later
    /// failure of its initialization is then supervised like any crash, and
    /// the [`start_timeout`](Self::start_timeout) the dropped wait carried no
    /// longer bounds the first start.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorError, ActorResult, SpawnError, SupervisionConfig,
    /// };
    ///
    /// #[derive(Default)]
    /// struct Worker;
    ///
    /// impl Actor for Worker {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn pre_start(&mut self, _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Err(ActorError::user("no config"))
    ///     }
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    /// }
    ///
    /// struct Boss;
    ///
    /// impl Actor for Boss {
    ///     type Message = ();
    ///     type Response = bool;
    ///     async fn handle(&mut self, _: (), ctx: &mut ActorContext<Self>) -> ActorResult<bool> {
    ///         let started = ctx.spawn_child(Worker::default).named("w").await_init().await;
    ///         Ok(matches!(started, Err(SpawnError::Init(_))))
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let boss = Boss
    ///     .spawn()
    ///     .with_supervision(SupervisionConfig::one_for_one())
    ///     .await?;
    /// assert!(boss.send(()).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn await_init(mut self) -> Self {
        self.await_init = true;
        self
    }
}

/// The supervisor's report of a `spawn_child` it refused before any child
/// existed: `named` is the child's name, when it has one.
fn report_refused(
    supervisor: &ActorId,
    named: Option<&ActorId>,
    err: &SpawnError,
    sensitive: bool,
) {
    let unnamed = ActorId::from("?");
    reports::start_error(
        supervisor,
        named.unwrap_or(&unnamed),
        None,
        || StopReason::Failure(ActorError::Spawn(err.clone())),
        sensitive,
    );
}

impl<'ctx, A: Actor, F, C: Actor> std::future::IntoFuture for ChildSpawnBuilder<'ctx, A, F, C>
where
    F: Fn() -> C + Send + Sync + 'static,
{
    type Output = Result<ActorHandle<C>, SpawnError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'ctx>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let ChildSpawnBuilder {
                ctx,
                factory,
                name,
                restart_type,
                shutdown,
                config,
                start_timeout,
                kill_grace,
                runtime,
                significant,
                await_init,
                sys,
                _child,
            } = self;
            let sensitive = sys.effective_sensitive::<C>();
            let supervisor = ctx.actor_id().clone();
            // The refusals of the setup itself come before any factory runs
            // and before anything is registered; the refusals of the spawn
            // (not a supervisor, stopping, a taken name) keep their own order
            // and are answered first.
            ctx.refuse_setup(name.as_deref(), significant, restart_type, config.as_ref())
                .inspect_err(|err| {
                    let named = name.as_deref().map(ActorId::from);
                    report_refused(&supervisor, named.as_ref(), err, sensitive);
                })?;
            if let Some(placed) = &runtime {
                // The refusals come first, as in `stop_within`: a spawn that
                // one of them refuses needs no timer and never panics.
                let named = name.as_deref().map(ActorId::from);
                ctx.refuse_spawn(named.as_ref()).inspect_err(|err| {
                    report_refused(&supervisor, named.as_ref(), err, sensitive);
                })?;
                require_runtime_timers("ChildSpawnBuilder::on_runtime", placed);
            }
            if await_init && start_timeout.is_some() {
                let named = name.as_deref().map(ActorId::from);
                ctx.refuse_spawn(named.as_ref()).inspect_err(|err| {
                    report_refused(&supervisor, named.as_ref(), err, sensitive);
                })?;
                require_caller_timers("ChildSpawnBuilder::await_init");
            }
            let name: Option<Arc<str>> = name.map(Arc::from);
            ctx.await_retired_subtree(name.as_ref())
                .await
                .inspect_err(|err| {
                    let named = name.as_ref().map(|name| ActorId::from_shared(name.clone()));
                    report_refused(&supervisor, named.as_ref(), err, sensitive);
                })?;
            let (handle, wait) = ctx.spawn_child_internal(
                factory,
                name,
                restart_type,
                shutdown,
                config,
                start_timeout,
                kill_grace,
                runtime,
                ChildFlags {
                    significant,
                    await_init,
                },
                sys,
            )?;
            match wait {
                None => Ok(handle),
                Some(wait) => ctx.finish_await_init(handle, wait, start_timeout).await,
            }
        })
    }
}

/// The options of a child spawn that the spec records or the call awaits.
#[derive(Clone, Copy)]
struct ChildFlags {
    significant: bool,
    await_init: bool,
}

/// What a spawn with `await_init` waits on: the child's start
/// acknowledgment, and what cuts the start at a bound.
struct InitWait {
    ack: oneshot::Receiver<Result<(), ActorError>>,
    child_id: ActorId,
    incarnation: u64,
    lane: StopLane,
    abort: tokio::task::AbortHandle,
}

/// Waits for a start acknowledgment until `expired` completes. When the bound
/// passes first, the channel is read once: a value there means the child
/// acknowledged, and the start succeeded or failed as it answered; a sender
/// dropped without a reply is the init's own end, as it is when the end is read
/// before the bound. Only with nothing to read is the channel closed and read
/// again: a value that landed before the close wins, and otherwise the child's
/// acknowledgment cannot be delivered after that (its send fails from the close
/// on), and `cut` runs. A child whose answer or end the first read finds is
/// never cut with a start timeout.
pub(super) async fn bounded_ack(
    ack: &mut oneshot::Receiver<Result<(), ActorError>>,
    expired: impl Future<Output = ()>,
    cut: impl FnOnce(),
) -> InitEnd {
    tokio::select! {
        biased;
        answer = &mut *ack => init_end(answer),
        () = expired => match ack.try_recv() {
            Ok(answer) => init_end(Ok(answer)),
            Err(oneshot::error::TryRecvError::Closed) => InitEnd::Closed,
            Err(oneshot::error::TryRecvError::Empty) => {
                #[cfg(test)]
                before_close_hook();
                // The close wakes a sender that waits for it, inside a catch.
                contain_waker_panic("start ack close", None, || ack.close());
                match ack.try_recv() {
                    Ok(answer) => init_end(Ok(answer)),
                    Err(_) => {
                        cut();
                        InitEnd::TimedOut
                    }
                }
            }
        },
    }
}

#[cfg(test)]
fn before_close_hook() {
    if let Some(hook) = BEFORE_CLOSE.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

/// Reads the answer of a start acknowledgment.
fn init_end(answer: Result<Result<(), ActorError>, oneshot::error::RecvError>) -> InitEnd {
    match answer {
        Ok(Ok(())) => InitEnd::Started,
        Ok(Err(err)) => InitEnd::Failed(err),
        Err(_closed) => InitEnd::Closed,
    }
}

/// How the wait of a spawn with `await_init` ended.
pub(super) enum InitEnd {
    Started,
    Failed(ActorError),
    /// The acknowledgment's sender dropped without a reply.
    Closed,
    TimedOut,
}

/// [`build_for_attempt`] for a child that is `sensitive` or not: its log
/// lines print no text of a panic when it is.
#[cfg_attr(not(feature = "tracing"), allow(unused_variables))]
pub(super) fn build_for_attempt_of<F, A>(
    factory: &std::sync::Weak<F>,
    io: &SlotIo,
    supervisor: &ActorId,
    child_id: &ActorId,
    incarnation: u64,
    sensitive: bool,
) -> Result<A, Box<RestartOutcome>>
where
    F: Fn() -> A,
{
    let Some(factory) = factory.upgrade() else {
        io.mark_cancelled();
        return Err(Box::new(RestartOutcome::Cancelled {
            child_id: child_id.clone(),
            incarnation,
            drop_panic: None,
        }));
    };
    let built = catch_sync(|| factory());
    let Some(last) = Arc::into_inner(factory) else {
        return built.map_err(|payload| {
            let msg = payload_into_string(payload);
            Box::new(RestartOutcome::Failed {
                child_id: child_id.clone(),
                incarnation,
                reason: StopReason::Failure(SupervisionError::FactoryFailed(msg).into()),
            })
        });
    };
    let drop_panic = release(last);
    #[cfg(feature = "tracing")]
    if let Some(payload) = &drop_panic {
        caught_log!(warn, target: "tokio_actors::actor::context",
            supervisor = %supervisor,
            child = %child_id,
            incarnation,
            panic = %crate::telemetry::LogFields::of_sensitive(sensitive).panic_text(payload),
            "a restart attempt held the last reference to its child's factory, whose spec was removed during the build, and the factory's drop panicked; the panic is the supervisor's"
        );
    }
    let other = match built {
        Ok(actor) => release(actor),
        Err(payload) => Some(payload_into_string(payload)),
    };
    #[cfg(feature = "tracing")]
    if let Some(payload) = &other {
        caught_log!(warn, target: "tokio_actors::actor::context",
            supervisor = %supervisor,
            child = %child_id,
            incarnation,
            panic = %crate::telemetry::LogFields::of_sensitive(sensitive).panic_text(payload),
            "a restart attempt's build, or the drop of what it built, panicked after its child's spec was removed; the attempt is cancelled"
        );
    }
    #[cfg(not(feature = "tracing"))]
    let _ = (other, supervisor);
    io.mark_cancelled();
    Err(Box::new(RestartOutcome::Cancelled {
        child_id: child_id.clone(),
        incarnation,
        drop_panic,
    }))
}
