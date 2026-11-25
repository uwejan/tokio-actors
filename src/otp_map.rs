//! Where each Erlang/OTP facility lives in this crate.
//!
//! Each row names an Erlang/OTP facility and its counterpart here, or the
//! reason it has none. Where behavior differs, the item's own docs say how;
//! the shapes of the signal calls are on the `signals` page.
//!
//! # Processes
//!
//! | Erlang/OTP | Here |
//! |---|---|
//! | `spawn/1-4`, `spawn_opt` | `ActorExt::spawn` gives a `SpawnBuilder`; awaiting it starts the actor and returns once `pre_start` and `on_started` have succeeded |
//! | `spawn_link`, `spawn_monitor` | `SpawnBuilder::linked(&mut ctx)`, `SpawnBuilder::monitored(&mut ctx, map)` |
//! | `spawn_request` | the spawn future is `Send`: spawn it on a task and `notify` the result back (the README's Patterns table) |
//! | `self/0` | `ActorContext::self_handle`, `ActorContext::actor_id` |
//! | `is_process_alive/1`, `processes/0` | `ActorHandle::is_alive`, `ActorSystem::actor_status`, `ActorSystem::actor_ids` |
//! | `register/2`, `whereis/1`, `registered/0` | `SpawnBuilder::named`, `ActorSystem::get`, `ActorSystem::registered` |
//! | `{via, Module, Name}` | a map of the application's own from its keys to `ActorId`, resolved with `ActorSystem::get_by_id` |
//! | The process dictionary | the actor's own typed fields |
//! | `process_flag(trap_exit, _)` | `ActorContext::trap_exit(map)`, `ActorContext::untrap_exit` |
//! | `process_flag(sensitive, _)` | `Actor::SENSITIVE`, `SpawnBuilder::sensitive` |
//! | `proc_lib:set_label/1` | `ActorContext::set_label` |
//! | `hibernate_after`, `erlang:hibernate` | the trigger is `ActorContext::idle_after` with `Actor::on_idle`; there is nothing to reclaim, because a parked actor is a task waiting on its queue with no stack to discard; an actor that wants memory back releases what its fields hold from `on_idle` (`examples/idle_shrink.rs`) |
//!
//! # Signals
//!
//! | Erlang/OTP | Here |
//! |---|---|
//! | `link/1`, `unlink/1` | `ActorContext::link`, `ActorContext::link_priority`, `ActorContext::unlink` |
//! | `monitor/2,3`, `demonitor/1,2` | `ActorContext::monitor`, `ActorContext::monitor_priority`, `ActorContext::demonitor`, `ActorContext::demonitor_flush`; the notice is a `Down` |
//! | `exit/2` | `ActorContext::exit`; `exit(Pid, kill)` is `ActorHandle::stop(StopReason::Kill)`, `ActorHandle::stop_within` or `ActorSystem::kill_by_id` |
//! | `alias/0,1`, `unalias/1`, `exit/3` with `priority` | `ActorContext::alias(AliasOptions { reply, priority })`, `ActorContext::unalias`, `ActorContext::exit_priority` |
//! | Priority messages | a priority lane (`ActorConfig::with_priority_lane`), `ActorHandle::notify_priority`, and `ActorConfig::with_priority_by_alias`, which leaves the lane to the receiver's priority aliases |
//!
//! # Messages and timers
//!
//! | Erlang/OTP | Here |
//! |---|---|
//! | `!` | `ActorHandle::notify`, `ActorHandle::try_notify`; the mailbox is bounded and its capacity is set with `ActorConfig::with_mailbox_capacity` |
//! | `receive ... after` | `ActorContext::receive_timeout` |
//! | Selective receive | none, by design: see "No counterpart" |
//! | `erlang:send_after`, `start_timer`, `cancel_timer`, `timer:send_interval` | `ActorContext::schedule(msg)` with `after`, `at` or `every`, and `ActorContext::every_with`, a recurring timer taking a `MissPolicy`; `ActorContext::cancel_timer` |
//! | `timer:send_after` to another process, `timer:kill_after`, `erlang:read_timer` | patterns built from Tokio's own timers, listed in the README's Patterns table |
//!
//! # gen_server
//!
//! | Erlang/OTP | Here |
//! |---|---|
//! | `start_link` and `init/1` | `Actor::pre_start` and `Actor::on_started`; `SpawnBuilder::start_timeout` bounds the start |
//! | `init/1` returning `ignore` | `ActorContext::ignore`, called in a `pre_start` that returns `Ok(())`: the actor runs no `on_started`, no message loop and no `on_stopped`, ends with `StopReason::Graceful`; a root spawn and `ChildSpawnBuilder::await_init` answer `SpawnError::Ignored`, and a supervisor neither restarts the child nor calls `on_child_stopped`, keeping its spec, down, unless the child is temporary or a pool member (`restart_child` starts it again, `delete_child` removes it; a `spawn_child` under the same id answers `DuplicateChild`). `Application::start`, `StateMachine::init` and event manager handlers have no `ignore` |
//! | `call/2,3`, `cast/2` | `ActorHandle::send`, `ActorHandle::send_timeout`, `ActorHandle::notify` |
//! | `handle_call`, `handle_cast`, `handle_info` | the one `Actor::handle`; timers, streams, `Down`, `LinkExit` and replies arrive as the actor's own `Message` |
//! | `{noreply, S}` with `reply/2` | a `tokio::sync::oneshot::Sender` carried in the message and answered from a later turn (the README's Patterns table) |
//! | `send_request`, `wait_response`, `check_response` | `ActorContext::request`, `ActorContext::request_within`, `ActorContext::cancel_request`, `ActorContext::set_request_cap`; the reply arrives as a `RequestReply` |
//! | `{continue, C}`, `handle_continue/2` | `ActorContext::continue_with` |
//! | The `Time` action | `ActorContext::receive_timeout` |
//! | `terminate/2` | `Actor::on_stopped(reason)` |
//! | `format_status/1` | `Actor::format_status` |
//! | `gen_server:stop/1,3` | `ActorHandle::stop`, `ActorHandle::stop_within` |
//! | A callback that returns its next state | `Behavior` and `BehaviorActor` |
//!
//! # gen_statem and gen_event
//!
//! | Erlang/OTP | Here |
//! |---|---|
//! | `gen_statem` (`handle_event_function`, `state_enter`, `postpone`, time-outs) | `StateMachine`, `StatemActor::new`, `StatemContext::postpone`; `Harness` drives one without a runtime |
//! | `gen_event` | `EventManager`, `EventManagerRef::of`, `ActorContext::add_sup_handler` |
//!
//! # supervisor
//!
//! | Erlang/OTP | Here |
//! |---|---|
//! | The `supervisor` behaviour | `SupervisionConfig` on any actor (`with_supervision`), children spawned with `ActorContext::spawn_child` |
//! | Child spec: restart, shutdown, significant | `RestartType`, `Shutdown`, `ChildSpawnBuilder::significant`, `ChildSpawnBuilder::start_timeout` |
//! | Strategies, intensity and period | `RestartStrategy`, `SupervisionConfig::max_restarts` |
//! | `start_child`, `terminate_child`, `restart_child`, `delete_child`, `which_children`, `get_childspec`, `count_children` | `SupervisorRef::of(&handle)` with the same seven calls |
//! | An ordered initial start that fails as a unit | `SupervisionConfig::with_strict_start`, `ChildSpawnBuilder::await_init` |
//! | `auto_shutdown` and significant children | `AutoShutdown`, `SupervisionConfig::with_auto_shutdown` |
//! | `supervisor_bridge` | a pattern listed in the README's Patterns table |
//!
//! # Applications
//!
//! | Erlang/OTP | Here |
//! |---|---|
//! | The `application` behaviour, start types, dependencies | `Application`, `Applications` with `start`, `ensure_all_started`, `stop`, `which_applications` |
//! | A permanent application ending the node | `Applications::node_shutdown`, which the binary awaits; the crate never exits the process |
//! | `init:stop` | `ActorSystem::shutdown` |
//!
//! # sys, proc_lib and logging
//!
//! | Erlang/OTP | Here |
//! |---|---|
//! | `sys:get_status` | `ActorHandle::get_status` |
//! | `sys:get_state`, `sys:replace_state` | `ActorHandle::sys()` with `inspect` and `replace_state`, opened by `SpawnBuilder::allow_state_access` (`sys` feature) |
//! | `sys:suspend`, `sys:resume` | `suspend` and `resume` on `ActorHandle::sys()` |
//! | `sys:statistics`, `log`, `trace`, `install`, the `debug` start option | `set_statistics`, `statistics`, `set_log`, `log`, `set_trace`, `install` and `remove` on `ActorHandle::sys()`; `SpawnBuilder::debug(DebugOptions)` |
//! | `sys:log_to_file` | a function given to `install` that hands each debug entry, through a channel, to a writer the application owns |
//! | Crash reports, supervisor reports, `logger` | the `tracing` feature: the binary chooses the subscriber, levels and filters |
//!
//! # Monitoring and introspection
//!
//! | Erlang/OTP | Here |
//! |---|---|
//! | `erlang:system_monitor` with `long_schedule` and `long_message_queue` | `ActorSystem::system_monitor` with a `MonitorConfig` (`telemetry` feature) |
//! | `process_info` links, monitors, label, parent, `message_queue_len`; observer | `ActorSystem::snapshot`, `ActorHandle::mailbox_len`, `Applications::which_applications` |
//! | `erlang:statistics`, `memory/0`, `system_info/1` | `ActorSystem::actor_ids` with the runtime's own metrics |
//! | `pg`: scopes, groups, `monitor/2`, `monitor_scope/1` | `ActorSystem::pg`, `ActorSystem::pg_scope`, `PgScope::join`, `PgScope::leave`, `PgScope::members`, `PgScope::members_any`; `ActorContext::monitor_group`, `ActorContext::monitor_scope` |
//! | (no Erlang/OTP counterpart) the crate's typed events | the `telemetry` module: `ActorSystem::attach_telemetry`, `ActorContext::subscribe` |
//!
//! # No counterpart
//!
//! These facilities have no counterpart, for one of three reasons.
//!
//! ## Needs the Erlang VM or its scheduler
//!
//! | Erlang/OTP | Why none, and what covers the need |
//! |---|---|
//! | Process priorities (`process_flag(priority, _)`) | a Tokio task has no priority; the priority lane orders messages inside one actor only |
//! | Preemption by reductions | a task yields only at an await, so a turn that never yields holds its worker; long work goes to `spawn_blocking` or `block_in_place` (the README's Blocking Work section) |
//! | `max_heap_size` and the `large_heap` monitor event | one process shares one heap, so no allocation is counted per actor; the bounded mailbox caps queued messages, and `examples/flood_guard.rs` polls `ActorSystem::snapshot` and stops an actor whose mailbox stays long |
//! | VM tracing (`erlang:trace`, `trace_pattern`, `dbg`, `seq_trace`) | compiled code cannot be traced call by call at run time; the `telemetry` events and the `sys` trace cover an actor's lifecycle and messages |
//! | Process internals (`process_info` stack trace, memory, reductions, messages, heap sizes) | a parked future has no stack to read and the queued messages cannot be read in place; `ActorSystem::snapshot` gives lengths and counts |
//! | Hot code loading (`code_change`, `sys:change_code`, appup, relup) | Rust code is compiled ahead of time; a new version is a new build, deployed by restarting |
//!
//! ## Needs a network layer
//!
//! | Erlang/OTP | Why none |
//! |---|---|
//! | Distribution: nodes, `{global, Name}`, `monitor_node`, `noconnection`, `rpc`, `multi_call`, distributed applications | the crate is local to one process and has no networking layer |
//! | Remote addresses, location transparency | an `ActorHandle` reaches an actor of the same process only |
//!
//! ## Refused by design, with the substitute
//!
//! | Erlang/OTP | Why refused, and the substitute |
//! |---|---|
//! | Selective receive | an actor takes one typed `Message` through one `handle`, in order per lane; a receive that skips messages leaves them in the mailbox, and once skipped messages fill a bounded mailbox the sender of the awaited message waits for room that never comes, a deadlock. Substitutes: `StatemContext::postpone` on a state machine, which retries an event after the next state change, and a stash kept in the actor's fields (`examples/stash.rs`) |
//! | ETS and kin (`ets`, `dets`, `persistent_term`, `atomics`, `counters`) | Rust's shared types fill the need (`Arc` with a lock, atomics, a concurrent map), shared by the actors that hold them. A table held in an actor's fields ends with its owner, and the owner can hand it to an heir from `on_stopped` (`examples/heir_table.rs`); `on_stopped` does not run under `Kill`, so a killed owner hands nothing over, where an ETS heir receives the table whatever ended its owner |
