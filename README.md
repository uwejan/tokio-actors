# Tokio Actors

[![Crates.io](https://img.shields.io/crates/v/tokio-actors.svg)](https://crates.io/crates/tokio-actors)
[![Documentation](https://docs.rs/tokio-actors/badge.svg)](https://docs.rs/tokio-actors)
[![CI](https://github.com/uwejan/tokio-actors/workflows/CI/badge.svg)](https://github.com/uwejan/tokio-actors/actions)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust Version](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)

**An OTP-style actor runtime for Rust: supervision, lifecycle and crash semantics traced to Erlang/OTP, with the known departures listed, running as zero-ceremony Tokio tasks.**

Tokio Actors is an actor framework for Rust developers who want Erlang/OTP-style failure handling without the ceremony. Every actor's run loop is a dedicated `tokio::task` on your runtime. No custom schedulers, no macros, no hidden magic. What sets it apart is what happens when things go wrong: supervision, restarts, and cleanup are traced to Erlang/OTP's documented semantics; the departures known so far are listed below.

---

## Why OTP's Failure Semantics Matter

The actor model's production value was never the mailbox API. It is the failure semantics. Erlang/OTP's "let it crash" philosophy works because the runtime makes hard guarantees: every process death produces an exit signal that cannot be lost, supervisors stop and restart children in a documented order, cleanup (`terminate/2`) runs even when the process is dying from an exception, and a supervisor that exhausts its restart budget escalates with a reason its own parent knows how to interpret. Tokio Actors is built for the failure semantics: the supervision and lifecycle behavior is traced to Erlang/OTP's documented behavior (`gen_server`, `supervisor`), and the known departures are listed under Liveness Model below. When the docs here say a child restarts, that holds under a real crash - a panicking handler, not just a polite `Err`.

## The Guarantees

What happens when things go wrong. Every row is exercised by the test suite (see [Testing](#testing)):

| Failure semantic | tokio-actors guarantee |
|---|---|
| Panic in a handler | Caught at the callback boundary; the actor stops with `StopReason::Failure(ActorError::Panic)` and `on_stopped` still runs with that reason (gen_server terminate-on-exception parity) |
| Death notification | A runner task in the supervisor's death plane awaits the child's task and records its fate, and the supervisor's run loop delivers a `ChildEvent` to `on_child_stopped` for every child death that ends before the supervisor's message loop ends; a death still pending when the supervisor itself stops for any reason other than `Kill` is delivered once before `on_stopped` with `SupervisionAction::SupervisorStopping`, and one stopped with `Kill` delivers none. A child the supervisor's own teardown stops is not reported to `on_child_stopped`; with the `tracing` feature its end is logged as an error when it is not orderly |
| Automatic restart | Runtime-managed: OneForOne, OneForAll, RestForOne, SimpleOneForOne |
| Group restart ordering | A group restart stops its members one at a time in reverse start order, each ending, or being given the Unresponsive verdict, before the next is signalled, and initiates restarts in start order, each after the previous member re-registered (OTP ordering); a member whose stop was already in progress keeps stopping beside them, and a `terminate_child` of a member the group has not yet reached, or a `stop_child` of a `Transient` or `Temporary` one, stops that member at once, beside the member the group is stopping, which an OTP supervisor never does. A SimpleOneForOne supervisor stops its children concurrently, each under its own `Shutdown` policy. |
| Sequential restart init completion | A group restart awaits each member's full init handshake before initiating the next: sequential, not atomic. A mid-group init failure aborts the remaining starts and retries the group, charged once to the restart budget: a `OneForAll` retry stops and restarts every child, a child started during the group included, and a `RestForOne` retry the failed member and every child after it, so the members started before the failed one keep the incarnation the first attempt gave them (OTP parity). A member released by `terminate_child` stays down through every retry, a departure listed below. The supervisor keeps answering its own messages throughout |
| Manual child management | `terminate_child` / `restart_child` / `delete_child` / `stop_child` modeled on OTP's `supervisor` calls (answers that differ are listed under the deliberate departures below): spec kept (except for a `Temporary` child, or a child of a `SimpleOneForOne` supervisor, whose spec a manual stop removes), manual stops never charge the budget and complete under the child's `Shutdown` even if the caller abandons the wait. `terminate_child` succeeds for any known child, one already down or whose restart decision is still owed included, and cancels a pending restart, except that a child whose restart attempt is in flight answers `ChildRestarting`, and an actor of its subtree that never yields makes it answer `ChildUnresponsive` with the child left down; it returns only once the child's whole subtree has ended, a child already stopped whose actors are still ending included. `stop_child` returns `StopChildOutcome::Absorbed` when a pending group restart has still to stop and restart that `Permanent` child, a retry that will restart it again included, and `ChildRestarting` once that group has restarted it for good or released it, or while its own restart attempt is in flight; it stops a `Transient` or `Temporary` member of a pending group restart as `terminate_child` does, returning `StopChildOutcome::Stopped`. A `terminate_child` made while a group restart is pending keeps that child down through every retry of that group. A same-name `spawn_child` after `delete_child`, or after a `Temporary` or pool child was removed, waits until the previous child's subtree has ended. A failed `restart_child` is reported through `on_child_stopped` with `Removed` and never charges the budget |
| Restart budget escalation | Sliding-window budget; exhaustion stops the supervisor with the OTP `shutdown` reason (`StopReason::ParentRequest`), so a grandparent's Transient policy does not restart it |
| Forced termination | `Shutdown::Timeout` -> `Kill` -> task abort ladder; every stop path is bounded except a documented `Infinity`, a bare `stop()` (its `pre_stop` may keep vetoing, and once it is accepted nothing escalates it, so its `on_stopped`, final deliveries and child teardown run without a bound; `stop_within` is its bounded form), and a stop that reaches a child still initializing under `Infinity` without a `start_timeout` |
| Enumeration & kill | `actor_ids()`, `actor_status(id)`, and `kill_by_id(id)` reach every actor registered in a system, named or anonymous, until its registration ends (after its children are stopped, or, when it ends by `Kill`, without waiting for them: as its links drop, a worker child that does not trap exits is raised `Kill` and a supervisor child or a worker that traps exits `ParentKilled`, which can come after the id has left), before it reads `Stopped`; a `Kill` from `kill(name)`, `kill_by_id` or `stop(StopReason::Kill)` cannot be vetoed and no lifecycle hook begins once the target reads it, and one the target accepts while it is registered ends in the abort backstop after the target's kill grace (`kill_grace` on its spawn builder, 120 ms by default, zero aborting at once); a callback that never yields survives the abort, and `kill_by_id` reports it `Unresponsive`; `kill_by_id` reports the killed actor's own end, and an observer taken with `observe_subtree(id)` before the kill and awaited with `wait_ended()` afterwards waits for the actors below it too (`kill_by_id_with_observer(id)` takes the observer and the kill from one lookup) |

**Pairing rule.** A parent's `Shutdown` covers its children only when it is at least the sum of their stop budgets (each child's `Shutdown`, plus its kill grace, plus the short wait that confirms an abort), or the largest of them for a `SimpleOneForOne` pool. A root supervisor stopped by `ActorSystem::shutdown` gets `per_actor_timeout` (5 s by default) for its whole teardown. A shorter window escalates the parent to `Kill`, and its children still running then take the path of a killed supervisor's children: workers that do not trap exits receive `Kill`, and a child that is itself a supervisor, or a worker that traps exits, stops in order within its own `Shutdown`. Erlang/OTP states a stronger rule for supervisor children: "If the child process is another supervisor, the shutdown time must be set to infinity to give the subtree ample time to shut down." (`supervisor` documentation, child specification).

**Limits.** Panic capture requires unwinding: `panic = "abort"` builds have no in-process supervision (the process dies). A handler stuck at an `.await` point is force-killable, but a non-yielding busy loop is beyond even task abort; it surfaces as a typed `SupervisionError::ChildUnresponsive` after a bounded wait instead of hanging the supervisor: from a manual stop, and as the stop reason of a supervisor whose pending group restart can only go on through it. With the `tracing` feature, each verdict logs one warning when it is made, once per incarnation whichever route reaches it: the child's stop ladder, or a restart attempt's, `terminate_child`'s or `spawn_child`'s wait for the subtree that incarnation left running. Where a call waits for the child, the same verdict also reaches it as `ChildUnresponsive`, and where none does (a restart attempt, or a supervisor's own teardown) the child is not restarted while it lives. A restart attempt whose wait for the previous incarnation's subtree expires instead stops its supervisor, with that `ChildUnresponsive` as its failure, so the supervisor's own parent decides. A bare `stop()` can be vetoed by `pre_stop` indefinitely, and once it is accepted nothing escalates it; `stop_within` is the bounded form. A stop that reaches a child still initializing under `Shutdown::Infinity` without a `start_timeout` waits for its initialization without a bound. `Shutdown::Infinity` children can stall a group restart indefinitely, matching OTP's own infinity semantics. A child factory that never returns blocks its supervisor, as an OTP start function that never returns blocks its supervisor. And there is no distribution layer by design: tokio-actors is local-first (see [Non-Goals](#non-goals)). Failure semantics traced to OTP are the product, not a stepping stone to a cluster framework.

Every actor shares one OS process and one heap: unwinding panics and `Err` stay inside the actor, but a `panic = "abort"` build, a stack overflow, a panic raised while another unwinds, an allocation failure, `std::process::abort` and `std::process::exit` end every actor at once, with no `on_stopped` and no supervisor report. There is no per-actor memory limit; a bounded mailbox caps only the number of messages queued for that actor, not their size. Actors share the runtime's worker threads, so a callback that never yields holds its worker: on a current-thread runtime it delays every task, on a multi-thread runtime it delays the tasks that stay on that worker (see Blocking Work). An actor's own fields are reachable only from its task. A message type must be `Send + 'static`, which still admits shared state such as `Arc<Mutex<T>>`, and handler code can reach statics and thread-locals; state shared that way is shared.

---

## Lifecycle Contracts

Every lifecycle promise is a mechanism the runtime enforces, not a documentation convention:

| Contract | Guarantee |
|---|---|
| Truthful spawn | `spawn().await` acks through `pre_start` AND `on_started` before it resolves, with an infinity default and an opt-in `.start_timeout(dur)`. A failed init returns `SpawnError::Init` instead of dying silently, or `SpawnError::Killed` when a `Kill` reaches the actor before its end is final; `.detached()` provides fire-and-forget spawning for callers who want it. |
| Uniform crash semantics | `Err` from `handle` stops the actor the same way whether the message arrived via `send` or `notify` - one crash path, not two. Recoverable conditions belong in the `Response` type as `Ok` values; crash cleanup belongs in `on_stopped`, which receives the `StopReason`. |
| Observable lifecycle | `ActorHandle::status()` and `wait_stopped()` form a runtime plane, a per-actor cell the runtime writes directly: `status()` answers without the actor's help even when an actor is stuck at an `.await` point, and `wait_stopped()` resolves when the actor ends, an abort included; `get_status()` serves the richer queue-plane snapshot. |
| Bounded, truthful shutdown | `ActorSystem::shutdown()` stops roots only, in reverse registration order, so each supervisor tears down its own subtree: it takes every root actor through its stop ladder to a terminal state, or to the `Unresponsive` verdict for a root that does not end even when aborted, and returns a `ShutdownReport`: `outcomes` for the roots, `swept` for any non-root leftover the final sweep handles (normally empty): each leftover is waited on, bounded by the global deadline (the earlier of the call's `ShutdownPolicy::timeout`, taken as no earlier than the moment the sweep begins, and the deadline the system already holds from another shutdown's sweep; the system drops the held deadline when the last shutdown in flight ends, provided a shutdown has completed since the system was last `Active`), and the ones still running then are force-stopped; after the deadline a shutdown can run on for the forced roots' longest kill grace plus the abort settle time, then the forced leftovers' longest kill grace plus the abort settle time, and, for either of those two steps, that time once more plus the abort settle time when the abort of an actor it forces, scheduled with the system, is issued late. A leftover whose task ended without publishing `Stopped` reads `Aborted`. One that published `Stopped` reads `Killed` when its end became final at or after the deadline (or carries no instant) or when the shutdown's force took a `Kill` on it, and `Graceful` when it became final before the deadline. A leftover reads `Unresponsive` when even the abort did not end it within the settle time after it. With a root to wait on, it needs a Tokio runtime with the time driver enabled that is driven, not only entered; a system with no root completes without the time driver, and with no runtime current only if it also has no leftover. The walk runs in a task of the system on the runtime current at the call, and registrations are refused from the call's first poll: it survives a caller that is dropped, abandoned or never polled again, and a call made from inside an actor that the walk stops (that actor is stopped like any other root, and the walk goes on). It does not survive its own runtime: if that ends first and nothing is still waiting on the call, the system keeps refusing registrations, and `reactivate` works again once a shutdown of the system has completed (an earlier one counts) and none is in flight; a caller still waiting walks the system itself, with the same needs. |
| Matchable errors | `ActorError` carries structured `Timer`/`Stream`/`Supervision` variants instead of stringified messages, `send_timeout` spans one call-wide deadline (`AskError::Timeout { enqueued }`: `enqueued: false` means the request never reached the mailbox and is safe to retry; `true` means the deadline raced the reply, retry only if idempotent), and every error enum is exhaustive. |

In the same spirit: one-shot schedules are lazy and `#[must_use]` (a dropped builder never fires), `.every_with(factory)` covers recurring messages that are not `Clone`, and `ActorSystem::get_by_id` resolves an `ActorId` to a live handle.

---

## Liveness Model

An actor runs until it is stopped or killed (by a caller, by its supervisor or by a system shutdown), until it fails (an `Err` or a panic from one of its callbacks ends it), or until its runtime ends; dropping its handles does not end it. While it is registered, every actor is enumerable and killable through its system (`kill_by_id` needs a calling runtime with the time driver when the id is registered and its actor takes the `Kill`). Erlang/OTP gives a process's lifetime structure through links, monitors and `supervisor` trees (erlang.org/docs/28); this crate provides supervision trees, links and monitors, and structuring actors under supervisors is still the recommended way to manage their lifetimes: a supervisor learns of its children's deaths through `on_child_stopped`, a watcher learns of an actor's end through `monitor`, and any holder of a handle can await `wait_stopped()` for the actor's stop reason.

Internal self-references - a recurring timer schedule, a stream forwarder, `self_handle()` used inside a callback - never extend an actor's lifetime, and observers (status watchers, `wait_stopped()` futures, subtree observers) never root one either: process lifetime governs timer lifetime, not the reverse (Erlang/OTP's `timer` BIFs cancel a pid-addressed timer automatically when its target dies; erlang.org/docs/28).

The deliberate departures from Erlang/OTP, each documented rather than silently assumed:
- Initial sibling spawns during a supervisor's startup may overlap; only restarts are strictly sequential. By default a `spawn_child` does not await the child's initialization, so a child whose first `pre_start` or `on_started` fails ends with `StopReason::Failure` and is restarted under the budget like any crash (a `Temporary` child is removed), while its supervisor's own start succeeds, where an Erlang/OTP supervisor whose child fails to start stops the children already started and fails its own start; for a `spawn_child` after startup, `supervisor:start_child/2` is synchronous, returns the start error, keeps no spec and charges no intensity, where here `spawn_child` returns `Ok`, the child is restarted and the budget is charged, unless `.await_init()` is chained, which returns the start error, keeps no spec and charges no budget. `with_strict_start()` awaits every child spawned while the supervisor starts, and awaiting each child's `get_status()` in `on_started` gives a sequential strict start whose failure ends the spawn with the error `on_started` returns.
- A restarted child's init wait is unbounded by default under any `Shutdown`, matching OTP; `spawn_child(..).start_timeout(dur)` opts a specific child into a bound on each restart, and a restart whose init does not finish within `dur` is cut: `Kill` is raised on it and its task is aborted after its kill grace. `unbounded_start()` writes the default out, and the last call of the two wins. A plain `spawn_child` never awaits the first start, so the bound does not apply to it; with `.await_init()` it does.
- A supervisor keeps processing its own mailbox throughout a group restart; OTP's `supervisor` process is blocked for the whole operation.
- A stop can be vetoed by `pre_stop`. The veto is bounded by the Timeout -> Kill -> abort ladder except under `Shutdown::Infinity`, and a bare `stop()` that `pre_stop` keeps vetoing is unbounded, with `stop_within` its bounded form, where Erlang/OTP's `terminate/2` cannot refuse a stop.
- `Kill` is observed at the next turn boundary, so the `Kill` itself never interrupts a callback already begun; a callback still awaiting once the target's kill grace has passed is aborted at that `.await` by the abort backstop, where an Erlang/OTP kill acts anywhere in the execution flow.
- Every `Kill` accepted while its target is registered ends in an abort backstop after the target's own kill grace (120 ms unless its spawn builder sets another, zero allowed), and a loop that never yields survives it, where Erlang/OTP needs no backstop because a kill is certain. A target that has left the registry is in the last step of its end, waiting only for its timer and stream tasks to finish, and is not aborted; one that has sealed its stop lane refuses the `Kill`.
- A `Kill` sent after `spawn_child` returns can reach the child before its task first runs, because a plain `spawn_child` does not await the child's start (`.await_init()` does): that incarnation ends before any start hook begins and the child follows its restart type like after any other `Kill`, where in Erlang/OTP a `Kill` sent after `supervisor:start_child/2` returns cannot precede `init/1` of a `gen_server` child started with `start_link`. A counter in `pre_start` can therefore read fewer runs than the instances the child's factory built, so count in the factory; a `Kill` raised while `pre_start` runs skips `on_started`.
- Stop requests coalesce on one lane and are ranked inside a strength, where Erlang/OTP delivers every signal in per-sender order.
- A failure of `on_stopped` or of a final delivery, and a panic of `pre_stop`, does not replace an earlier failure as the exit reason (for `pre_stop`, a `Failure` stop request raised while it ran), where an Erlang/OTP `terminate/2` that raises makes its own exception the exit reason.
- A supervisor's loop never blocks on a start, restart or group stop that the supervisor makes itself, where Erlang/OTP's supervisor is blocked through each; only a handler holds it, by awaiting a manual stop (for `terminate_child`, the child's whole subtree included) or a same-name `spawn_child` that waits for a previous child's subtree, until that wait ends.
- Supervision is configuration of an actor, not a separate process, where an Erlang/OTP supervisor is a dedicated `gen_server`.
- `restart_child` returns before the child's init completes, and a failed manual start is reported through `on_child_stopped`, where Erlang/OTP's call is synchronous and returns the start error.
- A `spawn_child` made while a group restart is pending starts at once, and its init may overlap the group, where Erlang/OTP serves `start_child` after the restart, in the same process.
- Supervisor children default to `Timeout(5 s)` like any other child, governed by the pairing rule above, where Erlang/OTP defaults a supervisor child's shutdown to `infinity`, the value its documentation asks for.
- System shutdown signals roots only, each supervisor stopping its own subtree, with a per-root bound (5 s by default) and a global deadline (30 s by default) at which every actor still running is forced; past that deadline the call can run on for the forced actors' kill graces and the short wait that confirms an abort, where the Erlang/OTP application master waits without a bound.
- System shutdown (`shutdown` and `shutdown_with`) runs its walk in a task of the system on the runtime current at the call, where in Erlang/OTP `init:stop/0` is a request to a system process and `init` owns node shutdown; a dropped call does not stop the walk, which ends with its runtime at the latest, and a shutdown awaited inside a root's own handler completes, because ending that root does not end the walk. With no runtime current, the walk runs in the caller's future and completes only for a system with no root and no leftover; otherwise it panics at its first wait, since that wait needs a driven runtime with the time driver. `stop_within` is owned by the system from the call's first poll: a task of the system takes its `Kill` and abort steps whether or not the caller keeps waiting (on an actor runtime without the time driver, a lost stop task takes no step before its window has passed), as the supervisor runs a child's shutdown in `supervisor:terminate_child` when the caller's call times out, and where Erlang/OTP waits for the `'DOWN'` after its kill without limit the call reports `Unresponsive` after a short settle time. A call whose steps a task of the system runs (`start_timeout`, `stop_within`, and `shutdown` of a system with a root) needs a runtime that is driven, not only entered, and a bounded stop (`stop_within`, `kill_by_id`) of an actor that does not take the request is answered first (`Err(SendError::Closed)` and `None`), on any runtime; a cooperative `stop_within` finishes when the actor ends, whether or not the task is polled.
- A child that never yields gets one `Unresponsive` verdict, which escalates to its supervisor when the child blocks progress, and a group restart's one-at-a-time stop passes over a member given the verdict, so the next member is signalled while the unresponsive one may still run, and a member whose stop was already in progress keeps stopping beside them. Erlang/OTP waits for the `'DOWN'` after a kill without limit, so it stops a group's members one after another and never signals the next beside a live one. Such a child keeps its name while it runs, and can make each ancestor in turn stop itself.
- A restart, a `terminate_child` and a same-name `spawn_child` wait for the previous child's subtree to end, bounded by the stop budgets that subtree carries (a worker that traps exits adds its own `Shutdown` window from its call to `trap_exit`), except where a supervisor child with `Infinity` lies in it, or a worker with `Infinity` that traps exits (from its call to `trap_exit` until it ends), whose orderly stop under a killed parent has no bound; a `terminate_child` of a child already down waits for its old subtree the same way. Erlang/OTP has no such wait and avoids the race with `infinity`, the shutdown its documentation asks for on a supervisor child.
- A killed supervisor's supervisor children tear down in order within their own `Shutdown`, then take `Kill` and, once their own kill grace has passed, an abort (under `Shutdown::Infinity` nothing bounds that teardown, unless a `Kill` already stands on the child, which keeps its abort), where an Erlang/OTP child supervisor that traps exits runs its `terminate/2` without a bound under `infinity`. The `ParentKilled` stop that begins this ends a `Transient` child cleanly, so it is not restarted, as for `ParentRequest`, where an Erlang/OTP `transient` child is restarted on any exit reason other than `normal`, `shutdown` and `{shutdown, Term}`, so a killed end is abnormal for it.
- A worker takes its parent's orderly stop like a trapping process (`pre_stop` and `on_stopped` run). When its parent is killed, a worker that traps exits (`trap_exit`, read when the parent's end reaches it) is stopped in order, bounded by its own `Shutdown`, and a worker that does not trap exits receives `Kill`, as a non-trapping Erlang/OTP worker dies at once on any non-normal parent exit.
- Pool children (`SimpleOneForOne`) are each stopped under their own `Shutdown`, where Erlang/OTP uses one timer from the pool's single shared spec.
- The default restart budget is 3 restarts in 5 s, where Erlang/OTP's default intensity is 1 in 5 s. A zero `restart_window` is accepted, where Erlang/OTP rejects a zero period at start: a restart counts while it is no older than the window, inclusive, so a zero window exhausts the budget only when restarts read the same instant (for example on a paused clock that does not move between them), and a `max_restarts` of 0 exhausts it at the first restart the strategy would make.
- When a supervisor stops for any reason but `Kill`, each child death still pending when its message loop ended is delivered once to `on_child_stopped` with `SupervisionAction::SupervisorStopping` before `on_stopped` (no restart, no budget charge, `ctx.status()` reading `Stopping`), and supervision calls made from such a delivery are refused with `SupervisorStopping`; a `Kill` that arrives meanwhile ends the rest. These deliveries spend the supervisor's own `Shutdown` window (a root's `stop_within` window or the system shutdown's per-root timeout) and have no bound under `Shutdown::Infinity` or a bare `stop()`. A `Kill` delivers none, and an end during the teardown that follows has no callback. This goes beyond Erlang/OTP, whose supervisor delivers no exit to a callback once its termination began and only logs such an end as a `shutdown_error` report.
- A stop is taken at the next turn boundary, ahead of any child report or message queued before it: unless `pre_stop` vetoes it (only `Graceful`, `Shutdown` and `ParentRequest` can be vetoed), queued messages are dropped unhandled (a queued `send` returns `AskError::ResponseDropped`), and a child's end that arrived first is never evaluated before the stop. Erlang/OTP receives `gen_server:stop` as a system message in arrival order, so earlier messages are handled first, and a supervisor, trapping exits, handles that child's exit first; an OTP 28 `exit/3` with the priority option, sent to a priority alias of a receiver that traps exits, has the same shape for a stop. A row of Patterns below recovers OTP's order for a requested stop.
- A child's death, once the task that watches the child has reported it to the supervisor's death plane, is received ahead of the messages that child sent before it ended and that are still queued, because the death plane outranks the priority lane and the mailbox: `on_child_stopped` can run before `handle` sees the child's last messages, where Erlang/OTP delivers a child's last messages to a trapping parent before its exit by default; an OTP 28 parent that links with the priority option gets this order.
- Any holder of an actor's handle can send on its priority lane unless the actor leaves the lane to its priority aliases with `with_priority_by_alias()`, where Erlang/OTP lets a sender use priority only through an alias the receiver created for it, so the receiver chooses who may.
- `is_alive()` reads false from the moment the actor's message loop is done (the return of `on_stopped`, or the loop's end under a `Kill`, which skips that hook), through the teardown of the actor's children, while `status()` reads `Stopping` until `Stopped` (an actor whose loop ended with `Kill` keeps `Running` until `Stopped`), where Erlang/OTP's `is_process_alive/1` stays true until `terminate/2` returns. The mailbox stays open while `on_stopped` runs, so a message sent then is accepted and then dropped unhandled; a send after `on_stopped` returns is refused. `stop(StopReason::Kill)` returning `Ok` means the actor will end; `wait_stopped()` waits for the end and reads `Kill`.
- A `terminate_child`, or a `stop_child` of a non-`Permanent` member or of a non-`Permanent` child started since, made while a group restart is pending stops at once a member (or such a child) that the group's one-at-a-time stops have not reached, so two children may stop at once, and keeps that child down through every retry of that group. An Erlang/OTP supervisor serves `terminate_child` before or after one restart attempt, never between its one-at-a-time stops, and its own retry starts every child of the list again, except the member whose start failed, whose termination cancels the retry.
- A `stop_child` of a `Permanent` member of a pending group restart whose restart by that group is still ahead, a retry that will restart it again after another member failed to start included, returns at once as absorbed, so the call adds no incarnation to the ones the group gives the member, where Erlang/OTP's serial order would restart the member once more, unless it is the member whose start failed, whose termination between two attempts cancels the retry.
- During a pending group restart, a `stop_child` of a `Permanent` member that the group has restarted for good or released (a retry that will restart the member again absorbs the call instead), a `restart_child` of any member of the group (a released one included) and a `delete_child` of an awaited or queued member answer `ChildRestarting` until the group restart completes, a call made between two attempts of the group's retry included. An Erlang/OTP supervisor serves such a call between the attempts, because its retry is a message the supervisor sends itself: there the bounce's equivalent, a `terminate_child` then a `restart_child`, gives the member one more incarnation, and a `restart_child` or `delete_child` of a member already running again is answered `{error, running}`, except that for the member whose start failed Erlang/OTP also answers `{error, restarting}` and its termination cancels the retry. The caller can retry once the group completes.
- `restart_child` and `delete_child` answer `ChildRestarting` while the child's stop is in progress and while its group restart is pending (for `restart_child`, a member released by `terminate_child` included; `delete_child` answers such a member by its own state), where OTP's supervisor, which serves one call at a time, would not see those states.
- A child that died and whose restart decision is still owed (a crash a bounce met, or a death the supervisor has not handled yet) is answered `ChildRestarting` by `stop_child`, `restart_child` and `delete_child`: the supervisor makes that decision once and these calls neither make it nor replace it, where an Erlang/OTP supervisor answers `restart_child` and `delete_child` with `{error, running}` until it has processed the exit and has no bounce call. `terminate_child` of such a child dismisses the decision, keeps the child down, reports its death once to `on_child_stopped` as `Removed`, and returns once the child's whole subtree has ended, as OTP's `terminate_child` flushes the exit and answers `ok`.
- A spawn whose initialization failed and that a `Kill` then reached before its end was final reads `SpawnError::Killed`, the one answer its status plane and every observer also give, where an Erlang/OTP starter receives the initialization's error while its monitors see the kill.
- An actor whose runtime shuts down ends with `StopReason::Kill`, its task dropped with the runtime, where Erlang/OTP's nearest end is `noconnection`; for a child placed on a runtime of its own with `on_runtime`, when that runtime is gone, each restart attempt ends with `Kill` and is charged to the restart budget, until the budget ends the supervisor. A placed runtime that is alive but never driven runs no incarnation: a restart waits pending, charged once when it begins and not again while it waits, or the child gets the one `Unresponsive` verdict, which has no Erlang/OTP analog. On a runtime without the time driver the kill grace is lost, as the time driver paragraph under Spawning Actors says; a runtime handed to `on_runtime` must have it unless it has already ended. Erlang/OTP has no documented choice of which scheduler threads run a process, but it offers process priority and dirty schedulers; `on_runtime` approximates both (heavy work kept off the shared workers, a runtime on raised-priority threads) and keeps the placement across restarts.
- A `spawn_child` under a name whose child spec the supervisor holds is refused with `SpawnError::DuplicateChild` whether the child runs (one whose death the supervisor has not handled yet included), is being restarted or is down with its spec kept, where Erlang/OTP's `start_child` answers `{error, {already_started, Pid}}` for a running child or one whose exit it has not processed yet and `{error, already_present}` for the others; `children()` lists the specs the supervisor holds, and `delete_child` removes the spec of a down child.

---

## Feature Highlights

### OTP-Style Supervision
Supervision is crash-visible. A panic in a handler or lifecycle hook is caught and becomes `StopReason::Failure(ActorError::Panic)`. Supervisors detect every child death through a runner task that awaits the child's task, and the run loop delivers a `ChildEvent` to `on_child_stopped` for every death that ends before the supervisor's message loop ends (a child the supervisor's own teardown stops is not reported there). A death still pending when the supervisor itself stops for any reason other than `Kill` is delivered once before `on_stopped` with `SupervisionAction::SupervisorStopping`, and one stopped with `Kill` delivers none. Restart strategies come from Erlang/OTP:
- **OneForOne**: Restart only the failed child
- **OneForAll**: Restart all children when any one fails
- **RestForOne**: Restart the failed child and all children started after it
- **SimpleOneForOne**: Dynamic children sharing a single factory

Group strategies (OneForAll/RestForOne) follow OTP ordering: affected children are stopped in reverse start order, then restarted in start order (a `terminate_child` of a member the group has not yet reached, or a `stop_child` of a `Transient` or `Temporary` one, stops that member beside the one the group is stopping, a departure listed above). Children keep their `ActorId` and `ActorConfig` across restarts. Each child has a `RestartType` (Permanent/Transient/Temporary), and a sliding-window restart budget prevents restart storms. When the budget is exhausted, the supervisor stops with `StopReason::ParentRequest`, and its own supervisor's Transient policy will NOT restart it, matching OTP shutdown semantics.

### Lifecycle Observability
Two observability planes, after OTP's `sys:get_status` and `process_info` split; unlike `sys:get_status`, `get_status` is answered ahead of queued messages. The system channel gives a rich snapshot even when the mailbox is full:

```rust
let status = handle.get_status().await?;
println!("{}: {} children, {} timers", status.id, status.child_count, status.timer_count);
```

The runtime plane answers without the actor's help, even for an actor stuck at an `.await` point inside `handle`, and gives you an awaitable terminal state:

```rust
let phase = handle.status();   // ActorStatus, no channel round trip
handle.wait_stopped().await;    // resolves with the actor's stop reason once it reaches Stopped
```

### Strongly Typed
Message and response types are enforced at compile time. No runtime type casting, no `Any` trait abuse.

```rust
impl Actor for MyActor {
    type Message = MyMessage;   // Compile-time checked
    type Response = MyResponse;  // No guessing

    // ...
    async fn handle(&mut self, msg: MyMessage, _ctx: &mut ActorContext<Self>) -> ActorResult<MyResponse> { todo!() }
}
```

### Bounded Mailboxes = Natural Backpressure
A mailbox is bounded by default (64). When it is full, senders wait automatically, so a producer cannot outrun the actor without limit; a capacity of `usize::MAX` (clamped to tokio's semaphore maximum) makes it effectively unbounded.

### Timer Drift Handling (MissPolicy)
Recurring timers have three drift strategies to handle system lag:
- **Skip**: Jump to next aligned tick
- **CatchUp**: Send the missed ticks at once, pausing while the mailbox is full; no tick is lost
- **Delay**: Reset timer from now

This is the kind of edge-case thinking production systems need.

---

## Quick Start

```bash
cargo add tokio-actors
```

### Counter: The Basics

```rust
use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult, StopReason};

#[derive(Default)]
struct Counter(i64);

impl Actor for Counter {
    type Message = i64;
    type Response = i64;

    async fn handle(&mut self, msg: i64, _ctx: &mut ActorContext<Self>) -> ActorResult<i64> {
        self.0 += msg;
        Ok(self.0)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let counter = Counter::default().spawn().named("counter").await?;
    counter.notify(5).await?;          // fire-and-forget
    let total = counter.send(3).await?; // request-response -> 8
    counter.stop(StopReason::Graceful).await?;
    Ok(())
}
```

### Spawning Actors

Every spawn starts with `.spawn()` and chains options via `SpawnBuilder`:

```rust
use std::time::Duration;
use tokio_actors::{actor::ActorExt, ActorConfig, ActorSystem, SupervisionConfig};

// Each spawn below is an alternative. `spawn()` takes the actor by value, so `my_actor` is cloned,
// and a name is free again when its actor has stopped (`wait_stopped()` has returned).
// Anonymous (UUID auto-id, registers as a root in the default system)
let h = my_actor.clone().spawn().await?;

// Named (registered in default system)
let h = my_actor.clone().spawn().named("worker-1").await?;

// Named with custom mailbox
let config = ActorConfig::default().with_mailbox_capacity(256);
let h = my_actor.clone().spawn().named("worker-1").with_config(config).await?;

// On a specific system
let sys = ActorSystem::create("my-system")?;
let h = my_actor.clone().spawn().named("worker-1").on_system(&sys).await?;

// Supervisor (children restart via OneForOne, 3 restarts / 5s)
let h = my_actor.clone().spawn().named("supervisor").supervisor().await?;

// Supervisor with custom strategy
let sup = SupervisionConfig::one_for_all().max_restarts(10, Duration::from_secs(60));
let h = my_actor.clone().spawn().named("supervisor").with_supervision(sup).await?;
```

A runtime built without the Tokio time driver (`Builder::enable_time` or `enable_all`; `#[tokio::main]` and `#[tokio::test]` enable it) runs plain actors (actors with no timer and no supervised child): spawn, send and a graceful stop need no timer. Everything that sleeps needs the driver. On a runtime without it, `schedule` and `every_with` panic at the call, before anything is registered (a start hook that makes the call fails the start with the panic's text); a `Kill` aborts the task at once, with no kill grace; a call that bounds its wait on the calling runtime (`start_timeout`, `stop_within`, `kill_by_id`, `kill_by_id_with_observer`) panics before it changes anything, unless a rule that needs no timer refuses the call first (for a spawn, a taken name, a system that is shutting down or a zero mailbox capacity; for a stop or a kill, an actor that does not take the request or an id nothing holds); `SubtreeObserver::wait_ended` changes nothing and panics at the first moment it needs a timer, which is when the observed actor has ended and let go of everything it held and an actor below it has not ended (while the actor still runs or is still letting go of what it held it waits without a timer, and a subtree that has ended is answered without one, on any runtime or none); and `shutdown` of a system with a root panics at its first wait on that root, after the system has stopped taking registrations. With `panic = "abort"` the first timer of the crate on such a runtime ends the process. A panic hook that ends the process also ends it at a call that finds no time driver: the crate catches the probe's panic, but the hook runs before the catch.

### Actor Registry (ActorSystem)

```rust
use std::time::Duration;
use tokio_actors::{ActorSystem, ShutdownPolicy, SystemConfig};

// Default system (lazy singleton)
let sys = ActorSystem::default();

// Custom system with config
let sys = ActorSystem::create_with("game", SystemConfig {
    shutdown_policy: ShutdownPolicy {
        timeout: Duration::from_secs(60),
        per_actor_timeout: Duration::from_secs(10),
    },
    ..Default::default()
})?;

// The calls below assume actors spawned in "game": "worker-1" for the name calls (stop and kill
// are alternatives, as each removes it) and any actor for the id calls.

// Named lookup (OTP whereis/1)
let handle = sys.get::<MyActor>("worker-1");

// Stop/kill by name
sys.stop("worker-1").await?;   // Graceful (vetoable)
sys.kill("worker-1").await?;   // Force: no hook begins once the Kill is read

// Every actor, named or anonymous, is enumerable and killable by id
let ids = sys.actor_ids();
let status = sys.actor_status(&ids[0]);
let report = sys.kill_by_id(&ids[0]).await;   // Option<StopReport>: None when the id is not registered or its actor refuses the Kill; otherwise the target takes the same Kill, kill grace and abort as with kill(name), and this call also waits for its end (a short settle wait after an abort) to report it

// Coordinated shutdown: stops root actors only (each supervisor tears down
// its own subtree), takes every root through its stop ladder, and reports
let report = sys.shutdown().await;
```

A system stays in the process map of named systems (`get_named`, `all`, `create`) from its creation until a shutdown of it has completed. From then on the map holds it only while something else does: its user's `Arc`, or an actor of it that has not ended. When the last holder lets go, the system is freed, it is not found by name, and its name can be claimed again by `create`; its reaper tasks end once their pending entries have fired. `reactivate` puts a shut-down system back to staying in the map. The `default` system stays in the map for the life of the process.

---

## Core Concepts

### Supervision

Supervisors spawn children through their `ActorContext` and automatically handle restarts:

```rust
use std::time::Duration;
use tokio_actors::{
    actor::{Actor, ActorExt, context::ActorContext},
    ActorResult, ChildEvent, RestartType, Shutdown, SupervisionConfig,
};

struct MySupervisor;

impl Actor for MySupervisor {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        // Permanent - always restarts, even after graceful stop
        ctx.spawn_child(|| Worker::default()).named("worker").await?;

        // Transient - restarts on crash, stays down on graceful stop
        ctx.spawn_child(|| CacheActor::new())
            .named("cache")
            .restart_type(RestartType::Transient)
            .shutdown(Shutdown::Timeout(Duration::from_secs(10)))
            .await?;

        Ok(())
    }

    async fn on_child_stopped(&mut self, event: &ChildEvent, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        println!("Child {} stopped: {:?}", event.child_id, event.action);
        Ok(())
    }

    // ...
    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
}

// Launch with OneForAll strategy and custom budget
let sup = SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(30));
let handle = MySupervisor.spawn().named("my-sup").with_supervision(sup).await?;
```

The runtime handles the restart loop: evaluate strategy, check budget, invoke the factory, wire the new child in, all non-blocking. If the budget is exhausted, the supervisor itself stops with `StopReason::ParentRequest`.

When a child must be stopped (group restart, parent shutdown, or a manual stop), its `Shutdown` policy bounds the whole exchange: `Shutdown::Timeout(d)` requests a cooperative stop, escalates to `Kill` at expiry, and backs the `Kill` with a task abort once the child's kill grace has passed (`kill_grace` on its spawn builder, 120 ms by default). A child stuck at an `.await` point IS killable - abort cancels it at the next yield, and its drops still run. Only a handler spinning in a non-yielding busy loop is beyond reach; the supervisor waits a bounded time, then returns a typed `SupervisionError::ChildUnresponsive` to a manual stop, or stops with it when a pending group restart waits for the child, instead of hanging. With the `tracing` feature, each verdict logs one warning when it is made, once per incarnation whichever route reaches it: the child's stop ladder, or a restart attempt's, `terminate_child`'s or `spawn_child`'s wait for the subtree that incarnation left running. Where a call waits for the child, the same verdict also reaches it as `ChildUnresponsive`, and where none does (a restart attempt, or a supervisor's own teardown) the child is not restarted while it lives. A restart attempt whose wait for the previous incarnation's subtree expires instead stops its supervisor, with that `ChildUnresponsive` as its failure, so the supervisor's own parent decides.

**Kill grace.** Every actor has a kill grace: the time it is given, after a `Kill` is raised on it, to reach its next turn boundary before its task is aborted. It is 120 ms unless the spawn builder sets another (`.kill_grace(dur)` on `spawn()` for a root and on `spawn_child(..)` for a child), and a child keeps it across every restart. `Duration::ZERO` aborts the task as soon as the `Kill` is raised, so a callback in progress is cut at its next `.await` and never observes the `Kill` itself. The grace applies to every kill of the actor: `kill(name)`, `kill_by_id`, `stop(StopReason::Kill)`, the kill step of `stop_within` and of system shutdown, its supervisor's stop of it, and the kill a worker that does not trap exits receives when its own supervisor is killed. An actor that is itself a supervisor, or a worker that traps exits, stops in order when its supervisor is killed, bounded by its own `Shutdown`, and the grace applies to the `Kill` raised once that window has passed. `stop_within(StopReason::Kill, within)` waits its own `within` instead, for that call. After the abort, the drops of the actor's timers and streams must finish within the short wait that confirms it: a `Drop` that takes longer makes the actor read as one that does not yield, so a restart waiting for it ends with `ChildUnresponsive` and `kill_by_id` reports `Unresponsive`. The system schedules the abort when the `Kill` is accepted while the actor is still registered, so dropping the call that raised it never cancels the abort. Through an actor's handle (`stop`, `stop_within`) a `Kill` is accepted until the actor seals its stop lane, just before its queued messages and its value are dropped, so also while it runs `on_stopped` and stops its children, and after that it answers `SendError::Closed`; every other reason is accepted until `on_stopped` returns (until the loop's end when a `Kill` ended it) and refused with `SendError::Closed` from then on, and one raised during a supervisor's final deliveries or `on_stopped` is accepted and changes nothing; `ActorSystem::stop` and `kill` answer `SendError::NotFound` once the actor has left the registry, an aborted one included.

#### Managing Children

Supervisors can also drive child lifecycles manually, mirroring OTP's `supervisor` module. Manual stops are not failures: they never charge the restart budget and never trigger sibling (OneForAll/RestForOne) restarts.

| Call | What it does |
|------|--------------|
| `ctx.terminate_child(id).await` | Stop WITHOUT restart (OTP `terminate_child`). The child spec is kept (unless the child is Temporary or its supervisor is SimpleOneForOne) so the child can be revived later; until `delete_child` removes it, a same-name `spawn_child` is refused with `DuplicateChild`. Budget untouched. Succeeds for any known child, one already down or whose restart decision is still owed included: it dismisses the owed decision, cancels a pending restart, and returns once the child's whole subtree has ended. Errors with `ChildRestarting` only for a child whose restart attempt is in flight, and with `ChildUnresponsive` when an actor of the subtree never yields. |
| `ctx.restart_child(id)` | Revive a terminated child from its stored spec - same `ActorId`, same config (OTP `restart_child`). Errors with `ChildRunning` while the child's task runs, and with `ChildRestarting` while a restart attempt is in flight, the child's stop is in progress, its death awaits its restart decision, or its group restart is pending (a member released by `terminate_child` included). |
| `ctx.delete_child(id)` | Remove the child spec (OTP `delete_child`). Errors as `restart_child` does, except that a member released by `terminate_child` from a pending group restart is answered by its own state. |
| `ctx.stop_child(id).await` | Bounce per restart policy: Permanent children restart budget-free; Transient/Temporary stay down. Returns `StopChildOutcome::Absorbed` for a `Permanent` member of a pending group restart that has still to stop and restart it, a retry that will restart it again included, and `ChildRestarting` once that restart has restarted it for good or released it, or while the member's own restart attempt is in flight; a `Transient` or `Temporary` member of a pending group restart is stopped as by `terminate_child` and returns `StopChildOutcome::Stopped`. Errors with `ChildRestarting` for a child whose death awaits its restart decision. |

To force a restart, kill the child and let its restart policy restart it: call `ActorSystem::kill_by_id` with the child's id, or `stop_within(StopReason::Kill, d)` on its handle. An `ActorContext` has no `kill_by_id`, but `ctx.exit(&child_id, StopReason::Kill)` kills a registered actor by id (it cannot be trapped and is backstopped like a kill). A supervisor handler can also get the system with `ActorSystem::get_named`, passing the name the system was created with, and call `kill_by_id` on it, or call `stop_within(StopReason::Kill, d)` on a handle it kept. A Permanent child, and a Transient child (a Kill is an abnormal exit), is then restarted by its supervisor and the restart is charged to the budget, as Erlang/OTP does for `exit(Child, kill)`; a Temporary child is not restarted. `stop_child` is the budget-free bounce.

```rust
// Inside any supervising actor:
async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    match msg {
        // Take the worker down: no restart, no budget charge, spec kept
        Cmd::Pause => ctx.terminate_child("worker").await?,
        // Bring it back with the same ActorId and ActorConfig
        Cmd::Resume => ctx.restart_child("worker")?,
    }
    Ok(())
}
```

#### Stops Without Waiting

`ctx.begin_stop_child(id)` and `ctx.begin_terminate_child(id)` commit the stop that `stop_child` and `terminate_child` commit and return at once, without waiting for the child. The answer, the supervisor's record of the child and the report to `on_child_stopped` are the ones the awaited call gives, the end is reported once, and a begun `terminate_child` leaves the same record and report as an awaited one whose caller dropped its future. The child's slot stays taken until the child and every actor below it have ended: `delete_child` is refused while the stop is in progress, and a `spawn_child` under the same id is refused with `DuplicateChild` while the supervisor holds the spec, and waits for the subtree once the spec is gone. A significant child ended this way never shuts the supervisor down.

#### SupervisorRef

`SupervisorRef::of(&handle)` manages the children of a supervisor from outside it, with the calls of OTP's `supervisor` module: `start_child(factory, configure)`, `terminate_child(id)`, `restart_child(id)`, `delete_child(id)`, `which_children()` and `count_children()`. A call is answered at a boundary of the supervisor's run loop, so it does not queue behind the mailbox and is answered while the mailbox is full. A call from the supervisor's own task returns `SupervisorRefError::CallingSelf` at once, a supervisor that has ended gives `SupervisorRefError::Gone`, and a supervision error is carried as `SupervisorRefError::Supervision`. `start_child` registers the child with the options its `configure` closure sets and answers when the child is registered, and a panic in the factory or the closure fails the supervisor. `terminate_child` answers once the child and every actor it spawned have ended, and the stop completes if the caller drops its future. A call waits without a bound, as OTP's `supervisor` calls do; `SupervisorRef::of(&handle).timeout(d)` bounds every call made through the reference, a call that is not answered in time answers `SupervisorRefError::Timeout` (and `SupervisorRefError::NoTimeDriver` where the calling runtime has no time driver; with `panic = "abort"` a call on such a runtime ends the process, as the first timer of the crate does), and a request that was already queued is still served, so a child that then outlasts the whole of its stop ends the supervisor with `ChildUnresponsive`, as when a caller drops its future. `count_children` counts the specs, the active children (those with an incarnation alive; a child being restarted is not active), the supervisors and the workers.

#### Strict Start, Awaited Starts and Auto Shutdown

`SupervisionConfig::with_strict_start()` makes the supervisor's own spawn wait for the initialization of every child it spawned while starting. The children still initialize concurrently, the first child that fails fails the spawn with `SpawnError::Init` carrying `ActorError::ChildStart`, the children already started stop in reverse start order under their own `Shutdown`, and the supervisor gets neither `on_child_stopped` nor `on_stopped`. The wait has no bound of its own; the supervisor's `start_timeout` bounds it and a `Kill` ends it at once. A child that calls its supervisor during its own initialization, with a message or through a `SupervisorRef`, waits for a supervisor that waits for it until that bound, and a `SupervisorRef` call made during the wait is answered after it. Without strict start, a child whose initialization fails does not fail the supervisor's start.

`spawn_child(..).await_init()` makes the awaited builder return after the child's initialization and return its error when it fails: an `Err`, a panic or a start bound that passes. A failed child leaves no spec, delivers no `on_child_stopped` and charges no budget, and a spawn under the same id succeeds once the failed incarnation has ended.

`SupervisionConfig::with_auto_shutdown(mode)` with `spawn_child(..).significant(true)` shuts a supervisor down when a significant child ends in a way the supervisor did not order and does not restart: a `Transient` child's clean exit or a `Temporary` child's exit of any kind. `AutoShutdown::AnySignificant` shuts down at the first such end and `AutoShutdown::AllSignificant` at the end of the last active significant child; `Never` is the default. The supervisor delivers the child's end to `on_child_stopped` as `SupervisionAction::AutoShutdown`, then the deaths still pending as `SupervisorStopping`, and ends with `StopReason::ParentRequest`, so a `Transient` parent does not restart it. A stop the supervisor ordered never counts, whether awaited or begun without waiting, but a significant child started again that then ends cleanly on its own does. A significant child under `Never`, a significant `Permanent` child and a non-`Never` mode under `SimpleOneForOne` are refused with `SpawnError::InvalidSupervision`.

### Crash Semantics

Panics are crashes, not errors. A panic in `handle` or a lifecycle hook is caught at the callback boundary: the actor stops with `StopReason::Failure(ActorError::Panic)` (running `on_stopped` for post-init crashes, like gen_server's terminate-on-exception), and its supervisor (if any) restarts it per strategy. On the send path the caller reads the panic as an error once the actor's reason is final and its name is free (a `Kill`, an abort or the end of the actor's runtime that ends the actor first reads `ResponseDropped` with the final reason). `AskError` is flat - transport, actor, timeout, and self-call failures are peer variants - and carries `Timeout { enqueued }` from `send_timeout`:

```rust
use tokio_actors::{ActorError, AskError};

match worker.send(job).await {
    Ok(result) => println!("done: {result:?}"),
    Err(AskError::Actor(ActorError::Panic(msg))) => {
        // Handler panicked. The actor stopped with Failure(Panic) and its
        // name is free: look it up by name again to retry. A hit is the
        // next incarnation; nothing found is final unless a supervisor is
        // restarting the actor.
        eprintln!("worker crashed: {msg}");
    }
    Err(AskError::Actor(err)) => eprintln!("handler returned Err: {err}"),
    Err(AskError::Closed) => eprintln!("mailbox closed (actor down or mid-restart)"),
    Err(AskError::ResponseDropped { reason }) => {
        eprintln!("actor stopped before replying: {reason}")
    }
    Err(AskError::Timeout { enqueued }) => {
        // Only reachable via `send_timeout` - plain `send` has no deadline,
        // but the match must stay exhaustive since both share `AskError`.
        eprintln!("timed out (enqueued: {enqueued})");
    }
    Err(AskError::CallingSelf) => {
        // Called from the actor's own task, so nothing could answer.
        eprintln!("the actor called itself");
    }
}
```

On the notify path, `Err` and a panic behave identically to the send path: both stop the actor with `StopReason::Failure(..)`, `on_stopped` runs, and a supervisor (if any) restarts it per strategy. A notify caller never learns about the crash directly (fire-and-forget has no return channel), while a send caller reads it as `Err(AskError::Actor(..))` once the actor's reason is final and its name is free (`ResponseDropped` with the final reason when a `Kill`, an abort or the end of the actor's runtime ends it first). Recoverable outcomes are `Ok`-encoded values in `Response`.

**Known limitations**: `panic = "abort"` builds have no unwinding, so panic capture cannot exist there (the process dies); see `PANIC_CAPTURE_AVAILABLE`. The process ends the same way for a stack overflow, a panic raised while another unwinds, an allocation failure, `std::process::abort` and `std::process::exit`: no actor runs `on_stopped` and no supervisor is told. The default panic hook still prints to stderr even when supervision handles the crash. Set your own hook to silence it. A child with `Shutdown::Infinity` that refuses to stop can stall a group restart, matching OTP infinity semantics. The unbounded stop cases are a child under `Shutdown::Infinity`, a bare `stop()` (on a handle, or `ActorSystem::stop` by name), which `pre_stop` may keep vetoing and whose stop, once accepted, nothing escalates (`stop_within` is its bounded form), and a stop that reaches a child still initializing under `Infinity` without a `start_timeout`; every other stop path is bounded by the Timeout -> Kill -> abort ladder. A handler stuck at an `.await` point IS force-killable (the abort backstop cancels the task once its kill grace has passed); only a non-yielding busy loop remains beyond reach, and it surfaces as a typed `ChildUnresponsive` error after a bounded wait instead of hanging the supervisor (from a manual stop, or as the stop reason of a supervisor whose pending group restart waits for it). With the `tracing` feature, each verdict logs one warning when it is made, once per incarnation whichever route reaches it: the child's stop ladder, or a restart attempt's, `terminate_child`'s or `spawn_child`'s wait for the subtree that incarnation left running. Where a call waits for the child, the same verdict also reaches it as `ChildUnresponsive`, and where none does (a restart attempt, or a supervisor's own teardown) the child is not restarted while it lives. A restart attempt whose wait for the previous incarnation's subtree expires instead stops its supervisor, with that `ChildUnresponsive` as its failure, so the supervisor's own parent decides.

Further limits of the stop and restart paths:
- A `Kill` sent during `on_stopped` is observed when that hook returns, and a hook still awaiting once the actor's kill grace has passed (120 ms by default) is aborted at its next `.await`; the final reason is `Kill` either way.
- A supervisor's stop can take one extra child window when a child was restarting.
- An `Infinity` group member blocks the members queued behind it.
- An initialization that waits on a dependency being restarted waits without a bound, whatever the child's `Shutdown`, unless `start_timeout` bounds the restart's init; a stop ordered meanwhile is bounded by the child's `Shutdown` as usual.
- A restart after a supervisor's `Kill` waits for the subtree its previous incarnation left, bounded by the stop budgets that subtree carries, except as the next bullet says.
- A supervisor child with `Infinity`, or a worker with `Infinity` that traps exits, whose own supervisor is killed stops in order without a bound, so the restart of the killed supervisor waits for it without one until that child has ended (then for the rest of the subtree, bounded by its stop budgets). So does a `terminate_child` of any child whose subtree holds such a supervisor or such a worker, and so does a same-name `spawn_child` after `delete_child` or a removal while such a subtree is still ending: each of these holds the calling handler, and with it the supervisor's message loop, until that subtree ends.
- A stop ordered while a restarted child's acknowledgment waits for its supervisor is first signalled when the supervisor adopts it, after the supervisor's current callback, and `terminate_child` answers `ChildRestarting` while a restart attempt is in flight.
- A supervisor whose `pre_start` or `on_started` fails never ran its message loop and receives no `on_child_stopped` (with the `tracing` feature, a child that ended with an unexpected reason during that initialization is logged as an error).
- An error or panic in a final delivery makes the supervisor's exit reason `Failure` once `on_stopped` has returned, unless a `Failure` already stands as its exit reason or a `Kill` is raised before the stop lane is sealed (the final reason is then `Kill`), while the remaining deliveries still run unless that `Kill` ends them.
- A start cut short by its own `start_timeout` reads `Kill` from `wait_stopped()`, its supervisor's report carries the start timeout as its failure, and the caller of an awaited spawn reads `SpawnError::StartTimeout`.

### 3-Tier Termination

```rust
use tokio_actors::StopReason;

handle.stop(StopReason::Graceful).await?;   // Tier 1: pre_stop can veto
handle.stop(StopReason::Cancelled).await?;  // Tier 2: non-vetoable, on_stopped runs
handle.stop(StopReason::Kill).await?;       // Tier 3: not vetoable; no hook begins once it is read
```

### Bounded Stop

`stop()` raises a stop signal and returns immediately; nothing bounds how long `pre_stop` may veto it, nor, once it is accepted, how long the actor's `on_stopped` and the stop of its children take. `stop_within` bounds the wait and reports how far the escalation ladder had to go:

```rust
use std::time::Duration;
use tokio_actors::{StopOutcome, StopReason};

match handle.stop_within(StopReason::Graceful, Duration::from_secs(5)).await? {
    StopOutcome::Graceful => println!("stopped without this call's Kill taking effect"),
    StopOutcome::Killed => println!("stopped after this call's Kill was taken"),
    StopOutcome::Aborted => println!("its task ended without publishing Stopped"),
    StopOutcome::Unresponsive => println!("no terminal status within the call's last bound"),
}
```

This stops this incarnation only. A supervised actor's supervisor may restart it per its own policy the moment it reaches a terminal status; to stop a supervised child for good, use `ctx.terminate_child(id)` from the parent instead.

The ladder belongs to the system from the call's first poll, so a dropped call, or a caller that stops polling, does not stop it: for a reason other than `Kill`, a task of the system on the runtime that made the call raises the `Kill` at the end of the window and the abort follows after the kill grace; for `Kill`, the call raises it at once and the system aborts after the window. For an actor that still takes the request, the call needs a Tokio runtime with the time driver enabled that is driven, not only entered, and panics before it raises anything when none is current; an actor that does not take the request is answered with `SendError::Closed` first, on any runtime. A pending call with a reason other than `Kill` holds a small task until the call returns or, once it is dropped, until its ladder or its target ends, and a `Kill` request holds none; a call that is kept alive and never polled again holds it until its window has passed and its ladder has ended.

### Lifecycle Hooks

```rust
async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    // Initialize state, schedule timers
    ctx.schedule(HealthCheck).every(Duration::from_secs(30)).await?;
    Ok(())
}

async fn on_stopped(&mut self, _reason: &StopReason, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    // Cleanup resources
    self.database.close().await;
    Ok(())
}
```

`Actor::on_idle` is a further hook, run when the actor has been idle for the length set with `ctx.idle_after(d)` (see Idle Spells and Continuations).

### Message Passing: `notify` vs `send`

```rust
// Fire-and-forget (async until mailbox accepts)
handle.notify(msg).await?;

// Request-response (wait for actor to process)
let response = handle.send(msg).await?;

// Request-response bounded by a single deadline
let response = handle.send_timeout(msg, Duration::from_secs(2)).await?;

// Non-blocking attempt (returns immediately)
handle.try_notify(msg)?;
```

**Error Handling Nuance**:
- `Err` from `handle` stops the actor on **every** path - `notify`, `send`, and `send_timeout` alike. Encode recoverable outcomes as `Ok` values, and treat `Err` as a crash everywhere.
- Panics behave the same way, on every path (see Crash Semantics above).
- The only difference left is what the CALLER learns: `notify` cannot report a crash back (fire-and-forget has no return channel), while `send`/`send_timeout` surface it as `Err(AskError::Actor(err))` once the actor's reason is final and its name is free (a `send_timeout` deadline that passes first reads `Timeout { enqueued: true }`, and a `Kill`, an abort or the end of the actor's runtime that ends the actor first reads `ResponseDropped` with the final reason).

The messaging method you pick changes what the caller learns, never what the actor does.

Two actors that `notify` each other from `handle` through full mailboxes wait on each other for good, since a handler runs to its end before its actor receives again; `try_notify` answers `Full` and `send_timeout` answers `Timeout { enqueued: false }` to a caller outside the cycle. Two actors that `send` each other from `handle` park the same way, and `send_timeout` bounds the wait (`Timeout { enqueued: true }`). A message whose `notify` has returned is received before any message another actor notifies after that return; a `notify` still waiting for room promises no order. An actor that notifies itself from its own task through its own full mailbox gets `SendError::Full` at once.

### Priority Lane

A priority lane is a second, independent mailbox that four `*_priority` handle methods send on, received ahead of the normal mailbox. There is no default: enable it with an explicit capacity.

```rust
use std::time::Duration;
use tokio_actors::{actor::ActorExt, ActorConfig};

let config = ActorConfig::default().with_priority_lane(16);
let h = my_actor.spawn().named("worker-1").with_config(config).await?;

h.notify_priority(msg).await?;                                    // fire-and-forget
h.try_notify_priority(msg)?;                                       // non-blocking attempt
let response = h.send_priority(msg).await?;                        // request-response
let response = h.send_priority_timeout(msg, Duration::from_secs(2)).await?;

h.priority_len();       // 0 when no lane is enabled
h.priority_capacity();  // 0 when no lane is enabled
```

A priority message is received before every normal message still queued, including a normal message the same sender sent earlier; no message, priority or not, interrupts a callback already in progress. For example, a sender that calls `h.notify(a)` and then `h.notify_priority(b)` while the actor is inside a callback has `b` handled before `a`. Any holder of this actor's handle can send on the lane unless the actor leaves it to its priority aliases (see Aliases), where Erlang/OTP lets a sender use priority only through an alias the receiver created for it, so the receiver chooses who may. On an actor spawned without a priority lane, all four methods behave exactly like their normal-lane siblings, and `ActorStatusInfo` reports the lane's length and capacity separately from the normal mailbox's.

**Ordering.** The run loop receives, highest first: the stop lane, the system channel, the restart plane, the death plane, the signal of an Unresponsive verdict, the forwarder plane (timer and stream completions), the priority lane, and the normal mailbox. A child's death, once the task that watches the child has reported it to the supervisor's death plane, is received ahead of the messages that child sent before it ended and that are still queued, so `on_child_stopped` can run before `handle` sees them. Within a lane the messages one task sends keep their send order, and a stop request never travels on the priority lane.

**Starvation.** The lane's capacity bounds its memory and parks a sender that fills it, but nothing gives the normal mailbox a share: a priority flood at or above the rate the actor consumes it starves the normal mailbox for as long as it lasts, though no message is lost and the normal mailbox drains once the flood ends. A sustained storm of supervision events, which sit above the lane, delays the lane and the normal mailbox alike. Size the lane small and keep its traffic rare.

Erlang/OTP's own guidance applies: "You very seldom need to resort to usage of priority messages. Receiving processes have not been optimized for handling large amounts of priority messages." (Erlang Reference Manual, Processes)

### Requests

`ctx.request(&callee, msg, map)` asks another actor without waiting. It returns a `RequestId` at once, and the callee's response arrives later as one of the asker's own messages, which `map` makes from a `RequestReply { id, result }`. The `result` is exactly what `send` would have returned, and a callee that ends before it answers gives one reply, with `AskError::ResponseDropped` and its final reason. The reply takes an ordinary mailbox slot, so a full mailbox delays it like any message, and it follows every message the callee sent the asker before it answered.

`ctx.request_within(&callee, msg, d, map)` adds one deadline over the send and the wait: when it passes first, the reply carries `AskError::Timeout` and an answer that comes late is discarded.

`ctx.cancel_request(id)` drops a request's reply: `true` means no reply had been queued and none will arrive, `false` means one had been queued and is dropped instead of reaching `handle`. The callee is not told, and a request already in its mailbox is still handled.

At most as many requests as the asker's mailbox holds may be in flight. A request over the limit is refused with `RequestError::InFlightLimit`, `ctx.set_request_cap(n)` sets another limit (a cap of 0 is read as 1), and the count falls when a reply reaches `handle` or the request is cancelled. A request made while the actor is stopping is refused with `RequestError::Stopping`, and a reply still pending when the actor stops is dropped under every stop reason. `ActorStatusInfo::requests_in_flight` counts the requests made whose reply has not reached `handle` and that were not cancelled.

A panic in `map` fails the asker. A request to oneself cannot deadlock: the actor handles it after the current turn. A request to a supervisor's handle is a plain `send` to it. A reply is a message like any other, so it ends an idle spell, and a continuation its handler sets runs before the next queued message.

### Idle Spells and Continuations

An idle spell starts when the run loop is about to wait with its mailbox and priority lane empty, and ends when a message starts a turn. `ctx.receive_timeout(d, map)` makes the actor handle the message `map()` makes once the spell has lasted `d`, and `ctx.idle_after(d)` runs `Actor::on_idle` at `d` in the same way. Each fires once per spell and again in the next one; `cancel_receive_timeout` and `cancel_idle_after` clear them. Both setters need the Tokio time driver and panic without it, as every timer does.

A status read neither ends a spell nor restarts it, and neither does holding a link, a monitor, a group monitor, an alias or an outstanding request. The fire runs inline on the actor's task: it takes no mailbox slot and leaves the status `Running`. A message already queued is handled first and nothing fires. A length of zero fires once per spell and never spins. When both thresholds are due at one instant the receive timeout runs first. A stored `Kill` wins over the fire. A panic or an `Err` from the map, the hook or the handler ends the actor as one from `handle` does.

`ctx.continue_with(msg)` asks the actor to handle `msg` right after the current callback, before it reads any other message, request or stop. The step takes no mailbox slot and runs after the reply of the current request is sent. A step may continue again, and a status request waits until the chain ends. A stored `Kill` is read before each step, and a second call in one callback replaces the first. A message set in `pre_start` or `on_started` runs once the start is acknowledged, before the first message. The message is dropped, not handled, when the callback returns an `Err`, panics or stops its actor, when the start fails, when it is set in a callback that runs after the stop began, in `on_child_stopped` or in a `pre_stop` that vetoes the stop, and when the actor ends first.

### Aliases

`ctx.alias(AliasOptions { reply, priority })` makes an `Alias<A>`: a clonable name that sends to this actor, which the actor ends with `ctx.unalias(&alias)`. An alias sends with `notify`, `try_notify`, `notify_priority` and `try_notify_priority`. A send through an alias that `unalias` or its reply message ended returns `AliasError::Inactive` and the message is dropped, and one to an owner that has ended returns `AliasError::Closed`. When `unalias` returns `true`, no message of the alias enters the mailbox afterwards, and the messages queued before it stay and are handled. A reply alias takes exactly one message. An alias never keeps its actor alive and ends with its owner, and `ActorStatusInfo::aliases` counts the aliases that still accept messages.

A priority send through an alias rides the priority lane only for an alias made with `priority: true`. The gate is off by default; `ActorConfig::default().with_priority_lane(n).with_priority_by_alias()` leaves the lane to priority aliases, so the four `*_priority` methods of an `ActorHandle` send on the ordinary mailbox, as they do on an actor with no lane, and the receiver chooses who may use the lane.

`ctx.exit_priority(&alias, reason)` is `exit` through an alias. The signal rides the receiver's priority lane only for an active priority alias, a trapping receiver and a receiver with a lane; otherwise it behaves as `exit` does, through the mailbox for a trapping receiver and as an end for one that does not trap. A trapping owner with a priority lane that is linked to a peer takes the end of a peer linked with `link_priority` ahead of the messages queued before it, and the end of a plain link and an exit through a plain alias in queue order.

### Behaviors

A `Behavior` is a state that handles a message and names the state that handles the next. Its `handle(self, msg, cx)` is a plain `fn` that returns the next state with the response: `Next::Become(state)` replaces the state and `Next::Stop(reason)` ends the actor. `BehaviorActor::new(state)` runs one as an ordinary actor, so it is spawned, supervised and stopped like any other. The handler cannot await; it asks other actors with `cx.request`, and the answer arrives as a later message that the next state handles. `cx` also offers `alias` and `continue_with`.

A caller of the message that returned `Next::Stop` still gets the response, after the stop is sealed. `Next::Stop(StopReason::Kill)` is refused as `ctx.stop(StopReason::Kill)` is (`StopError::KillRefused`), and the actor then fails with a user error, and a panic in `handle` fails the actor like any handler panic.

### Timers with Drift Control

```rust
use tokio::time::Duration;
use tokio_actors::MissPolicy;

// `msg` is a Copy message value; pass `msg.clone()` for a Clone message that is not Copy, and use `every_with` for one that is not Clone.

// One-shot after delay
ctx.schedule(msg).after(Duration::from_secs(5)).await?;

// Recurring - default MissPolicy::Skip
ctx.schedule(msg).every(Duration::from_millis(100)).await?;

// Recurring with explicit drift strategy
ctx.schedule(msg).every(Duration::from_millis(100))
    .on_miss(MissPolicy::CatchUp)
    .await?;
```

The first tick of a recurring timer is due one interval after the call that registers it (the final `.await`); the miss policy governs the ticks after it.

**Edge Case**: Scheduling in the past? The message fires immediately. No panics, no silent failures.

### Mailbox Monitoring

`warn!` and `error!` are the `tracing` crate's macros, which your application depends on itself; the crate's own optional `tracing` feature is separate.

```rust
use tracing::{error, warn};

if handle.mailbox_available() < 10 {
    warn!("Actor {} is backed up!", handle.id());
}

if !handle.is_alive() {
    error!("Actor {} has stopped!", handle.id());
}

// System channel bypasses the mailbox, works even when full
let status = handle.get_status().await?;
```

### Turn Watchdog

Off by default. Turn it on to see how long each actor's current callback (`pre_start`, `on_started`, `handle`, `pre_stop`, `on_child_stopped`, `on_stopped`, `on_idle` or an inline signal map) has been running:

```rust
use std::time::Duration;
use tokio_actors::{ActorSystem, SystemConfig};

let config = SystemConfig::default().with_turn_warning_threshold(Duration::from_secs(1));
let sys = ActorSystem::create_with("game", config)?;

// Or, on any system, at any time:
sys.set_turn_warning_threshold(Some(Duration::from_secs(1)));

let ids = sys.actor_ids();
sys.turn_elapsed(&ids[0]);  // Some(d) while that actor is inside a callback that began after a threshold was set
sys.stuck_actors();         // every actor over the threshold, longest first
```

This is observation only: it never interrupts a callback or changes how a stop resolves, and `actor_status` still reports the status the actor holds (`Initializing`, `Running` or `Stopping`) for a wedged actor - `turn_elapsed` and `stuck_actors` are how to tell. While no threshold is set, `turn_elapsed` returns `None` for every actor and `stuck_actors` is empty. While one is set, each callback costs one extra monotonic clock read. With the `tracing` feature, a background check logs one warning for each callback it finds over the threshold. It runs every quarter of the threshold (never more often than every 50 ms, and at least every second). It runs on the runtime that was current when it started and ends with that runtime. It starts once a threshold is in effect and a runtime with the time driver is current: at the call that sets the threshold, or else at a later actor registration, and again the same way after its runtime was dropped. A callback that ends before a check finds it over the threshold is not logged, even if it ran longer than the threshold.

### Telemetry

The `telemetry` module defines typed events about the actors of a system: spawns and refused spawns, starts, stop requests, ends, supervision decisions, restart attempts, kill progress, helper tasks, turns, runs, timer ticks, waiting senders, stuck turns, mailbox backlogs, wait cycles and trace entries, and two about telemetry itself, detached handlers and dropped events. The subject of an event, when it has one, is an `ActorRef`: the actor's id, its incarnation and its ancestors.

```rust
use tokio_actors::telemetry::{Filter, KindSet, Scope, TelemetryEvent};

// A handler is a plain closure. It must not block: hand slow work to a channel.
let (tx, mut rx) = tokio::sync::mpsc::channel::<TelemetryEvent<'static>>(1024);
sys.attach_telemetry("export", KindSet::cold(), move |event| {
    let _ = tx.try_send(event.to_static());
});
// A task of yours reads `rx` and does the slow work there.

// A subscription, made inside a callback: the selected events arrive as this actor's own messages.
let id = ctx.subscribe(Filter::new(Scope::System), Msg::Event);

// A record of every registered actor that has not ended, read without asking any of them.
for record in sys.snapshot() {
    println!("{}: {:?}, mailbox {} of {}", record.actor.id(), record.status, record.mailbox_len, record.mailbox_capacity);
}
```

**Handlers.** `ActorSystem::attach_telemetry(id, kinds, handler)` attaches a handler, a plain closure, and `detach_telemetry(id)` removes it. A helper task of the system runs the handlers, with two exceptions: the turn event of a `handle` turn that went on, the run event and trace entries run them on the actor's own task between turns while the helper holds no event about that actor, and a `HandlerDetached` event runs the remaining handlers on the thread of the handler that panicked. A handler that blocks delays the events behind it, so hand slow work to a bounded channel with `try_send`, sending the owned copy `TelemetryEvent::to_static()` makes. An event that finds the helper's queue full is dropped and counted, and the count is reported to the handlers that take `Dropped` events.

**Subscriptions.** `ctx.subscribe(filter, map)` sends an actor the events it selects: `map` turns each `Delivery` into one of the actor's own messages. A `Filter` names a `Scope` (the whole system, one actor id, or one actor id and every actor below it) and a `KindSet`; turn, run, timer tick and trace events go to handlers only. Each subscription has its own bounded queue, so the code that raises an event never waits for the subscriber: an event that finds the queue full is dropped and counted, and the next `Delivery` carries the count in `missed_before`, or a `SubscriptionNotice::Overflow` notice carries it when the queue runs empty first. An actor is never sent an event whose subject is itself, and a subscription ends when its actor stops or restarts, when its `map` panics, or at `ctx.unsubscribe(id)`.

**Snapshot, monitor and switches.** `ActorSystem::snapshot()` reads an `ActorRecord` of every registered actor that has not ended without asking any of them, so a stuck actor does not delay it, and `ctx.set_label` names an actor in it. `system_monitor` makes one actor the system's monitor, told about long turns and long mailboxes as the monitor process of `erlang:system_monitor/2` is; a task of the system samples both, so a report comes some time after the crossing. `set_turn_tracking` and `set_wait_cycle_detection` (also `SystemConfig` fields) switch on turn timing and wait-cycle search, both off by default, and `is_quiescent` says whether a system is idle (`None` unless turn tracking is on and stays on during the call).

**Reports.** With the `tracing` feature, log statements about one actor carry `actor_id`, `actor_name`, `incarnation` and `system`, with the exceptions the `telemetry` module's docs name; each actor's task runs inside a debug-level span named `actor`, created at the spawn when a subscriber is interested in it; an actor that ends with a failure, or with its supervisor's death, writes a crash report on the `tokio_actors::crash` target, and a supervisor writes `child_terminated`, `start_error`, `reached_max_restart_intensity` and `progress` reports on the `tokio_actors::supervisor` target, as the reports of Erlang/OTP's supervisor are.

### Blocking Work

A `handle` invocation runs to completion before the actor observes anything else, so blocking inside it blocks that actor, and on a busy runtime it can starve its neighbors. Three tools cover it, all from the runtime you already own. For bounded CPU or blocking work, move it off the turn: `tokio::task::spawn_blocking(move || work()).await`. For a blocking call that needs `&mut self`, wrap it in `tokio::task::block_in_place(|| self.conn.execute(..))` (multi-threaded runtimes only). For an actor that is always busy, give it its own small runtime: an actor runs on whichever runtime is current when `.spawn().await` is called, so spawning it from inside a second runtime isolates it completely while abort, timers, and supervision keep working. A supervised child runs on its supervisor's runtime unless `spawn_child(..).on_runtime(&rt)`, with `rt` a `tokio::runtime::Handle`, places it on that runtime: every restart and every child of the child then runs there too, the supervisor keeps the stop ladder (the shutdown window, the kill grace and the settle) and the restart decisions, while a kill raised from outside that ladder waits its kill grace on the child's runtime, so that runtime needs the time driver. If the placed runtime is dropped, the child is killed and each restart fails and charges the restart budget; if it is alive but nothing drives it, no incarnation starts: `stop_child` and `terminate_child` answer `ChildUnresponsive` for a child spawned onto it once its window, kill grace and settle have passed, and a restart stays pending, charged once when it begins and not again while it waits, with `stop_child` and `terminate_child` answering `ChildRestarting` for it. To detect a wedged actor without any setup, probe it: `tokio::time::timeout(limit, handle.get_status())` times out when the actor has been inside one turn for longer than `limit`. A handler that never yields cannot be force-stopped by any of these; it surfaces as `ChildUnresponsive`. On a current-thread runtime it holds the only worker, so a neighbor gets no turn until it returns; on a multi-thread runtime it holds one worker and the others keep running.

```rust
// Inside any actor:
async fn handle(&mut self, msg: Job, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    tokio::task::spawn_blocking(move || run_job(msg))
        .await
        .map_err(|e| ActorError::user(e.to_string()))?;
    Ok(())
}
```

### Patterns

Recipes for Erlang/OTP behaviors the crate does not offer directly, built from the API above.

| To get | Pattern |
|---|---|
| `spawn_request` | Spawn without waiting: `tokio::spawn` the spawn future (it is `Send`) and `notify` its result back to the actor that asked. |
| `is_process_alive` after `exit(Pid, kill)` | `is_alive()` can read `true` right after `stop(StopReason::Kill)` returns, until the actor reaches its next turn boundary; await `kill_by_id`, `stop_within(StopReason::Kill, d)` or `wait_stopped()` when the end matters. |
| `via` registries | Keep a map from your own key to `ActorId` and resolve it with `get_by_id`. |
| `timer:send_after` to another process, `timer:kill_after` | `tokio::spawn` a `tokio::time::sleep`, then `notify`, `stop` or `kill_by_id`; a send to an actor that has stopped returns `Closed`. |
| `erlang:read_timer` | Keep the `tokio::time::Instant` passed to `at` and subtract `tokio::time::Instant::now()`: the result reads zero once the instant has passed, and it does not tell a cancelled timer from a pending one, where `erlang:read_timer` answers `false` for both an expired and a cancelled timer. |
| The process dictionary | The actor's own typed fields hold what a process dictionary would. |
| A deferred reply | Put a `tokio::sync::oneshot::Sender` in the message and answer it from a later turn. |
| A dedicated supervisor process | An actor whose only job is supervising (an empty `handle` and short hooks) is Erlang/OTP's supervisor shape: its supervision events never wait behind a long user turn. |
| An ordered initial start | In `on_started`, spawn each child and await its `get_status()`, which the child answers only after its `pre_start` and `on_started`; a child that calls back into the supervisor and awaits the answer during its initialization then waits forever, as an Erlang/OTP child does when its call to the supervisor has an infinite timeout; a `start_timeout` on the supervisor's own spawn ends the start with `SpawnError::StartTimeout`. |
| `supervisor:count_children`, `supervisor:which_children` | `SupervisorRef::count_children` counts specs, active children, supervisors and workers, and `which_children` or `ctx.children()` lists them; a `ChildInfo` carries `is_supervisor`. |
| A worker that cleans up when its supervisor is killed | Call `trap_exit` in the worker: when its supervisor is killed the worker is stopped in order with `ParentKilled`, within its own `Shutdown`, and runs `on_stopped`; a worker that does not trap exits receives `Kill` and begins no further hook. |
| `supervisor_bridge` | An actor whose `on_started` spawns the task and attaches `futures::stream::once(task)` over the task's `JoinHandle` as a one-item stream, whose `handle` turns the task's end into an `Err`, so the actor stops with `StopReason::Failure` (a `supervisor_bridge` stops with the subsystem's own exit reason), and whose `on_stopped` aborts the task. |
| Pipelined replies (the actor-model `become`) | Update state, hand the rest of the work to a task or a child actor, and take its result later as a message. |
| Kill latency, or turn atomicity | `kill_grace(Duration::ZERO)` gives Erlang/OTP's kill latency for a callback that awaits; a grace beyond any practical bound keeps every turn whole, and makes every wait that runs on that grace, its parent's stop budget included, last for the grace, which in practice is as long as under `Shutdown::Infinity`, while `stop_within(StopReason::Kill, d)` still aborts after `d`. |
| Locality | An actor reaches only the actors whose handles it holds, except through its system, which knows every registered actor by id: `get` and `get_by_id` return a typed handle to a registered actor of the type asked for (`get` only for a named one), `actor_ids`, `registered` and `actor_status` read the registry, `stop` and `kill` act on a named actor, and `kill_by_id` on any registered id. |
| Address identity | An actor's address is its handle's channels; its `ActorId` and name are aliases a restarted child keeps, so a handle to an ended incarnation and one to its successor compare equal: check `is_alive()` or resolve the id again with `get_by_id`. A caller that talks to a restartable actor resolves it by name (`get`) or by id (`get_by_id`) for each call instead of keeping a handle: a lookup that finds nothing is final for an actor that nothing restarts; for a supervised actor it means either that the next incarnation is not registered yet or that its supervisor will not restart it, so bound the retries, and a handle whose send returned `Closed` belongs to an ending incarnation, so wait for it with `wait_stopped` and look the actor up again. |
| Postpone, stash | Push the message onto a bounded queue in the actor's own fields and replay it from `handle` once the state allows; the queue sits outside the mailbox bound, so cap it yourself. |
| Delivery at the end | A message accepted while the actor runs a supervisor's final deliveries or `on_stopped` is dropped unhandled; from the return of `on_stopped` (the loop's end under a `Kill`) on, sends are refused. |
| A send that never blocks | `ActorConfig::default().with_mailbox_capacity(usize::MAX)` (clamped to `Semaphore::MAX_PERMITS`) makes `notify` never wait; the queue then grows without bound, so a flood can exhaust memory. |
| The `gen_server:stop` order | Send a `Stop` message whose handler calls `ctx.self_handle().stop(..)`, so earlier mail is handled first. |

---

## Deep Rust Patterns

### Recurring Timer Messages: `Clone`, Not `Sync`

Recurring timers do not require `A::Message: Sync`. The per-tick factory is a `Box<dyn FnMut() -> Message + Send>` owned by exactly one forwarder task, so `Sync` is never demanded. `.every(msg)` needs `A::Message: Clone` (the factory clones `msg` each tick); `.every_with(|| build_message())` drops the `Clone` bound entirely by calling your factory closure instead. One-shot schedules (`.at()`, `.after()`) need only `Send` - no `Clone`, no `Sync`.

### ActorHandle Equality

Handles implement `PartialEq` based on the `ActorId` and the system the actor was spawned in, not channel identity (a system that was shut down and dropped does not make a later system of the same name its successor, so their handles are never equal):

```rust
let actor1 = MyActor.spawn().named("foo").await?;
let actor2 = actor1.clone();

assert_eq!(actor1, actor2);  // Same actor ID

let actor3 = MyActor.spawn().named("bar").await?;
assert_ne!(actor1, actor3);  // Different actor ID
```

This allows handles to be used in `HashSet` and `HashMap` for deduplication and routing. Clippy's `mutable_key_type` lint fires on such a key because a handle holds channels, but its `Hash` and `Eq` read only the `ActorId` and the identity of its system, which never change, so allowing the lint at that site is sound.

### Bounded Mailbox Backpressure

When the mailbox is full:
- `notify().await` **blocks** until space is available, except from the actor's own task, where it answers `SendError::Full` at once
- `try_notify()` returns `TrySendError::Full` immediately
- `send().await` **blocks** (same as notify, just with response)

The default capacity is 64. A larger capacity is a per-actor opt-in through `ActorConfig::with_mailbox_capacity`, and `usize::MAX` (clamped to `tokio::sync::Semaphore::MAX_PERMITS`) makes the mailbox effectively unbounded: `notify` and `send` then never wait for room, so a producer that outruns the actor grows the queue until the process runs out of memory. Memory is taken as messages queue, and each queued message holds its value (and a reply channel for a send); size the capacity for the largest burst you accept to hold.

During a `MissPolicy::CatchUp` burst the extra missed ticks are sent without waiting for room. When the mailbox is full the burst pauses, and the timer then waits for room before it sends the pending tick, so no tick is lost and the actor is never blocked by its own timer.

---

## API at a Glance

### SpawnBuilder Chain

```rust
actor.spawn()                     // Start the builder
    .named("my-actor")            // Optional: assign a name/ID
    .on_system(&sys)              // Optional: target a specific ActorSystem
    .with_config(config)          // Optional: custom ActorConfig
    .supervisor()                 // Optional: supervise children (default config)
    .with_supervision(sup_config) // Optional: enable supervision (custom config)
    .start_timeout(dur)           // Optional: bound init instead of waiting forever
    .kill_grace(dur)              // Optional: time between a Kill and the abort (default 120 ms)
    .detached()                   // Optional, instead of .start_timeout: skip the init ack
    .await?;                      // Finalize: registers and spawns the task, then awaits pre_start + on_started (not when detached)
```

`.start_timeout(dur)` and `.detached()` both set how the spawn treats the init ack, and the one called last wins: chain one of them.

A root spawn can watch or link the new actor for the actor that spawns it, as Erlang/OTP's `spawn_monitor/1` and `spawn_link/1` do: chain `.monitored(&mut ctx, map)` (it resolves to the handle and a `MonitorId`), `.linked(&mut ctx)` (it resolves to the handle), or `.monitored(&mut ctx, map).linked()` for both, as the last call before `.await`. The monitor or link is made after the init ack, so an actor that ends at once is never missed, and a start that fails (`pre_start` or `on_started` returns `Err`, `start_timeout` expires, the name is taken) returns its `SpawnError` with nothing armed and no `Down` or exit signal following. A `.detached()` spawn has no ack to wait for, so its monitor or link is made as soon as the task is spawned. A spawn made by an actor that is stopping returns `SpawnError::OwnerStopping` before anything is spawned. A supervised child has no such form: after `spawn_child(..).await`, `ctx.monitor(&child, map)` or `ctx.link(&child)` is the same registration, and a child whose first start fails still sends its `Down`, because that start is not awaited.

A spawn waits for `pre_start` and `on_started` before it resolves, so spawning many actors one after another pays that wait once per actor. To start many at once, run the spawns together (`futures::future::join_all`, or a `tokio::task::JoinSet` that you keep to a bounded number in flight) so the waits overlap, and spawn from a task on the runtime rather than from a plain thread that blocks on each one. For an actor whose start failure nobody needs to hear about, `.detached()` resolves once the task is spawned; a failing `pre_start` is not reported to the spawn. A call that has not been answered when the actor ends gets no reply: one still queued is dropped without running, and one the actor was handling when a `Kill` ended its task is cut short, so a `send` returns `AskError::ResponseDropped` (the handler may have run in part); later calls return `Closed`, and `wait_stopped()` reports the end.

### ActorHandle Methods

| Method | Description |
|--------|-------------|
| `notify(msg)` | Fire-and-forget (awaits mailbox space) |
| `try_notify(msg)` | Non-blocking fire-and-forget |
| `send(msg)` | Request-response (awaits processing); a call from the actor's own task to itself returns `AskError::CallingSelf` |
| `send_timeout(msg, dur)` | Request-response bounded by one deadline spanning enqueue + response |
| `stop(reason)` | Stop via the stop lane (bypasses the system channel and a full mailbox alike) |
| `stop_within(reason, within)` | Stop bounded by `within`; reports `StopOutcome` (Graceful/Killed/Aborted/Unresponsive) |
| `get_status()` | Rich introspection snapshot via system channel (queue plane); it reports the actor's trap flag and the counts of its links, joined groups, group monitors, aliases and requests in flight |
| `status()` | Instant lifecycle phase from the runtime plane (answers even for a hung actor) |
| `wait_stopped()` | Awaits the actor's terminal state and returns its stop reason |
| `observe_subtree()` | Returns a `SubtreeObserver` for this incarnation's subtree, before or after the actor's end; its `wait_ended()` waits for the actor's end and then for every actor below it, and answers `Ended`, or `Unresponsive` when the stop budgets the subtree carries ran out with an actor still running; the answer is for the caller alone, and the wait records no verdict on any supervisor |
| `is_alive()` | Check if actor is still running: false from the moment its message loop is done (the return of `on_stopped`, or the loop's end under a `Kill`), through the teardown of the actor's children, while `status()` reads `Stopping` until `Stopped` (`Running` until `Stopped` when a `Kill` ended the loop) |
| `mailbox_len()` | Slots in use: queued messages plus slots senders have reserved and not yet filled |
| `mailbox_available()` | Free space in mailbox |
| `mailbox_capacity()` | Total mailbox capacity |
| `notify_priority(msg)` | Fire-and-forget on the priority lane (degrades to `notify` without one) |
| `try_notify_priority(msg)` | Non-blocking fire-and-forget on the priority lane |
| `send_priority(msg)` | Request-response on the priority lane |
| `send_priority_timeout(msg, dur)` | Request-response on the priority lane bounded by one deadline |
| `priority_len()` | Priority lane slots in use: queued messages plus reserved, unfilled slots (0 without a lane) |
| `priority_capacity()` | Total priority lane capacity (0 without a lane) |
| `id()` | Get actor ID |
| `incarnation()` | The incarnation token of this actor's current run; a supervised restart gives a new token |

### ActorContext Methods

| Method | Description |
|--------|-------------|
| `spawn_child(factory)` | Returns a `ChildSpawnBuilder` - chain `.named()`, `.restart_type()`, `.shutdown()`, `.kill_grace()`, `.on_runtime(&rt)`, `.with_config()`, `.start_timeout()`, `.unbounded_start()`, `.significant(bool)`, `.await_init()`; `.on_runtime` runs the child, every restart of it and everything it spawns on another Tokio runtime, which needs the time driver unless that runtime has already ended (without the driver, awaiting the builder panics before the factory runs or anything is registered, unless the call is refused first as not a supervisor, as made from a final `on_child_stopped` delivery of a stopping supervisor, or as a child name the supervisor already holds) |
| `children()` | Introspection info for all supervised children |
| `terminate_child(id)` | Stop a child WITHOUT restart (spec kept for later revival, unless the child is `Temporary` or its supervisor is `SimpleOneForOne`) |
| `restart_child(id)` | Revive a terminated child from its stored spec |
| `delete_child(id)` | Remove a terminated child's spec |
| `begin_stop_child(id)` | `stop_child` committed and returned at once, without waiting for the child; returns the same `StopChildOutcome` |
| `begin_terminate_child(id)` | `terminate_child` committed and returned at once, without waiting for the child |
| `stop_child(id)` | Stop a child, restarting per policy (budget-free bounce); returns `StopChildOutcome::Absorbed` for a `Permanent` member of a pending group restart that has still to restart it, `ChildRestarting` once that restart has restarted it for good or released it, and stops a non-`Permanent` member as `terminate_child` does |
| `schedule(msg)` | Returns a `ScheduleBuilder` - chain `.at(instant)`, `.after(delay)`, or `.every(interval)` |
| `cancel_timer(id)` | Cancel specific timer |
| `cancel_all_timers()` | Cancel all active timers |
| `active_timer_count()` | Number of active timers |
| `add_stream(stream)` | Attach an external stream to the mailbox |
| `cancel_stream(id)` | Cancel a specific stream |
| `cancel_all_streams()` | Cancel all active streams |
| `active_stream_count()` | Number of active streams |
| `self_handle()` | Get handle to this actor |
| `stop(reason)` | Stop this actor from inside its own callback; returns `Result<(), StopError>` |
| `monitor(target, map)` | Watch an actor by handle, id or name; `map` turns its `Down` into a message of this actor; returns `Result<MonitorId, MonitorError>` (`Stopping` once the actor is stopping) |
| `monitor_priority(target, map)` | `monitor` with the `Down` delivered on the priority lane (the normal lane without one) |
| `demonitor(id)` | Cancel a monitor; `true` means no `Down` will arrive for it, `false` for a foreign, unknown or spent id, or when the `Down` is already queued (it is still handled, and delivered once at a stop) |
| `demonitor_flush(id)` | `demonitor` that also drops a `Down` already queued; `false` when a flush was needed |
| `link(target)` | Link this actor to an actor by handle, id or name, as Erlang/OTP's `link/1`; returns `Result<(), LinkError>`, and linking an actor to itself does nothing. When the peer ends, a non-trapping actor ends with the peer's reason (`Graceful` is ignored), and a trapping one is handed a `LinkExit`. A link to an actor that has ended or does not exist reads `NoProc` |
| `link_priority(target)` | `link` with the trapped `LinkExit` delivered on the priority lane (the normal lane without one) |
| `unlink(target)` | Remove a link; `true` means no exit will arrive for it, `false` when it was not linked or its exit has already been queued |
| `trap_exit(map)` | Turn the exits of linked peers and `exit` signals into messages of this actor: `map` takes a `LinkExit` and runs contained; a trapping actor reads every reason, `Graceful` included |
| `untrap_exit()` | Stop trapping; exits arriving after the call take the non-trapping outcome |
| `exit(target, reason)` | Send an exit signal to an actor, as Erlang/OTP's `exit/2`: a trapping target gets a `LinkExit`, a non-trapping one ends with `reason`; `Kill` cannot be trapped, `Graceful` to another non-trapping actor is ignored (to oneself it ends the actor), and an ended or missing target is a no-op; a trapping target with as many exit signals waiting as its mailbox holds (64 for a supervisor that never called `trap_exit`) refuses it with `ExitError::Full` |
| `monitor_group(scope, group, map)` | Watch a process group of a `PgScope`: `map` turns each `GroupChange` into a message of this actor; returns `Result<(MonitorId, Vec<PeerRef>), MonitorError>`: the id and the current members |
| `monitor_scope(scope, map)` | Watch every group of a `PgScope`; returns `Result<(MonitorId, Vec<(Arc<str>, Vec<PeerRef>)>), MonitorError>`: the id and the current groups with their members. A subscriber whose queue overflows gets one `Resync` that replaces the changes it missed |
| `request(&callee, msg, map)` | Ask an actor without waiting; the answer arrives as the message `map` makes from a `RequestReply`; returns a `RequestId`, or `RequestError::Stopping` or `RequestError::InFlightLimit` |
| `request_within(&callee, msg, d, map)` | `request` with one deadline over the send and the wait; the reply carries `AskError::Timeout` when it passes first |
| `cancel_request(id)` | Drop a request's reply; `true` when none had been queued, `false` when a queued one is dropped instead of handled |
| `set_request_cap(n)` | Set how many requests may be in flight; the mailbox capacity when unset, and a cap of 0 is read as 1 |
| `receive_timeout(d, map)` | Handle the message `map()` makes once the actor has been idle for `d`, once per idle spell |
| `cancel_receive_timeout()` | Clear the receive timeout |
| `idle_after(d)` | Run `Actor::on_idle` once the actor has been idle for `d`, once per idle spell |
| `cancel_idle_after()` | Clear the idle length |
| `continue_with(msg)` | Handle `msg` right after the current callback, before any other message, request or stop |
| `alias(options)` | Make an `Alias<A>` that sends to this actor; `AliasOptions` has `reply` (takes one message) and `priority` (its priority sends ride the lane) |
| `unalias(&alias)` | End an alias; `true` means no message of it enters the mailbox afterwards |
| `exit_priority(&alias, reason)` | `exit` through an alias; rides the priority lane only for an active priority alias, a trapping receiver and a lane |
| `subscribe(filter, map)` | Receive the telemetry events `filter` selects as the messages `map` makes from each `Delivery`; returns a `SubscriptionId` |
| `unsubscribe(id)` | End a subscription; `false` for an id this actor does not hold |
| `set_label(label)` | Set a label for this actor, shown in a system snapshot |
| `actor_id()` | This actor's ID |
| `actor_name()` | This actor's registered name |
| `status()` | Current lifecycle status |

### Alias Methods

| Method | Description |
|--------|-------------|
| `notify(msg)` | Fire-and-forget through the alias (awaits mailbox space); `AliasError::Inactive` once `unalias` or a reply alias's message has ended the alias, `AliasError::Closed` once the owner has ended |
| `try_notify(msg)` | Non-blocking `notify` |
| `notify_priority(msg)` | Fire-and-forget on the priority lane for an alias made with `priority: true` on an actor that has a lane, on the ordinary mailbox otherwise |
| `try_notify_priority(msg)` | Non-blocking `notify_priority` |
| `is_active()` | Whether the alias accepts messages now |

### SupervisorRef Methods

| Method | Description |
|--------|-------------|
| `SupervisorRef::of(&handle)` | A reference to the supervisor the handle reaches; cloning is cheap |
| `start_child(factory, configure)` | Start a supervised child with the options `configure` sets on its `ChildSpawnBuilder`; returns its handle once it is registered |
| `terminate_child(id)` | Stop a child without restarting it; answers once the child and every actor it spawned have ended |
| `restart_child(id)` | Revive a terminated child |
| `delete_child(id)` | Remove a stopped child's spec |
| `which_children()` | The supervisor's children |
| `count_children()` | The `ChildCounts` of specs, active children, supervisors and workers |
| `timeout(d)` | The same reference with a bound on every call; a call not answered in time answers `SupervisorRefError::Timeout`, and the default has no bound |

### ActorSystem Methods

| Method | Description |
|--------|-------------|
| `ActorSystem::default()` | Lazy default system singleton |
| `ActorSystem::create(name)` | New named system |
| `ActorSystem::create_with(name, config)` | New system with custom config |
| `get::<A>(name)` | Typed actor lookup (OTP `whereis`) |
| `stop(name)` | Graceful stop by name |
| `kill(name)` | Force kill by name; the task is aborted once the target's kill grace has passed if it does not reach a turn boundary |
| `actor_ids()` | Snapshot of every registered actor id, named or anonymous |
| `actor_status(id)` | Current lifecycle status by id, or `None` if not registered |
| `kill_by_id(id)` | Force kill by id: the target takes the same `Kill`, kill grace and abort as with `kill(name)`, and this call also waits for its end, with a short settle wait after an abort; returns `Option<StopReport>`, `None` if the id is not registered or its actor refuses the `Kill`; for a registered id whose actor takes the `Kill`, the calling runtime needs the time driver, and the call panics before it changes anything without it; the report names the killed actor only |
| `kill_by_id_with_observer(id)` | `kill_by_id`, also returning a `SubtreeObserver` of the killed incarnation's subtree, taken from the same lookup that picked the target; `None` where `kill_by_id` answers `None` |
| `observe_subtree(id)` | Returns a `SubtreeObserver` for the registered actor's subtree, `None` when the id is not registered; take it before the kill, because a killed actor's id leaves the registry before the actors below it have ended |
| `shutdown()` | Roots-only coordinated shutdown; takes every root through its stop ladder (a root that does not end even when aborted is reported `Unresponsive`) and returns a `ShutdownReport` (`outcomes` for roots, `swept` for the non-root leftovers of the final sweep, each waited on, bounded by the global deadline (see `ShutdownPolicy::timeout`), and force-stopped if still running then; a system with a root needs the time driver; with a runtime current at the call the walk runs in a task of the system on it, so it survives a dropped call) |
| `registered()` | List all registered actor names |
| `pg()` | The default process-group scope of this system (named "pg"), as Erlang/OTP's `pg` default scope |
| `pg_scope(name)` | The scope of that name in this system; scopes are independent of each other and survive `reactivate` |
| `set_turn_warning_threshold(threshold)` | Turns the turn watchdog on (`Some(d)`) or off (`None`); observation only |
| `turn_warning_threshold()` | Current turn watchdog threshold, if any |
| `turn_elapsed(id)` | How long that actor's current callback has run; `None` between callbacks, for a callback that began before the threshold was set, or with the watchdog off |
| `stuck_actors()` | Actors over the threshold, longest first; empty with the watchdog off |
| `attach_telemetry(id, kinds, handler)` | Attach a handler, a closure called with the telemetry events of `kinds`; `false`, attaching nothing, when a handler is already attached under `id` |
| `detach_telemetry(id)` | Detach the handler attached under `id`; `false` when there is none |
| `snapshot()` | A read-only `ActorRecord` of every registered actor that has not ended, read without asking any actor |
| `system_monitor(&monitor, map, config)` | Make `monitor` the actor that is told about long turns and long mailboxes, as `erlang:system_monitor/2`; returns `Ok` with the settings it replaces or a `MonitorError`, and an empty `config` clears the monitor |
| `system_monitor_settings()` | The settings of the system's monitor, if it has one |
| `clear_system_monitor()` | Clear the system's monitor and return its settings |
| `set_turn_tracking(on)` | Turn turn tracking on or off; off by default |
| `set_wait_cycle_detection(on)` | Turn wait-cycle detection on or off; off by default; a cycle of waiting sends made inside hooks, none with a deadline, raises one `WaitCycle` event |
| `is_quiescent()` | Whether the system is quiescent now; `None` unless turn tracking is on and stays on during the call |

### PgScope Methods

| Method | Description |
|--------|-------------|
| `name()` | The scope's name |
| `join(group, &handle)` | Join an actor of this system to a group, as Erlang/OTP's `pg:join`; joins are counted per actor, so each needs its own `leave`. A join of an actor that has ended returns `PgError::NoProc`, and one of an actor of another system `PgError::ForeignSystem` |
| `leave(group, &handle)` | Remove one join of the actor; `PgError::NotJoined` when it holds none |
| `members::<A>(group)` | The group's live members as typed handles |
| `members_any(group)` | The group's live members as `PeerRef`s, whatever their type |
| `groups()` | The names of the scope's groups |

An actor leaves every group before it reads `Stopped`, so a member that has ended is never listed.

### ActorConfig Builder

`with_priority_by_alias()` leaves the priority lane to priority aliases (see Aliases).

```rust
ActorConfig::default()
    .with_mailbox_capacity(512)
    .with_priority_lane(16)               // Enable a priority lane; no default capacity
    .supervisor()                         // OneForOne, 3 restarts / 5s
    .with_supervision(SupervisionConfig::one_for_all()
        .max_restarts(10, Duration::from_secs(60)))  // Custom strategy + budget
```

---

## Testing

```bash
cargo test
```

Tests cover:
- Request-response and fire-and-forget messaging
- Timer drift policies (Skip, CatchUp, Delay)
- Mailbox backpressure and bounded capacity
- Handle equality and hashing
- Lifecycle hooks and 3-tier termination (a Kill skips every hook not yet begun)
- ActorSystem registry, spawn methods, and shutdown
- Supervision strategies, restart budget, child lifecycle
- Panic capture, crash restarts, and group restart ordering
- Stream integration (add_stream, StreamEvent, cancellation)
- SpawnBuilder chain options
- Error propagation and type preservation

---

## Examples

| Example | Description |
|---------|-------------|
| `simple_counter` | Basic notify/send usage |
| `ping_pong` | Bidirectional actor communication |
| `timers` | Recurring timer (default MissPolicy) |
| `cross_comm` | Multiple actors coordinating |
| `stream_counter` | External stream integration |
| `supervision` | Parent-child supervision with restart |

Run with:
```bash
cargo run --example supervision
```

---

## A Foundation for Agentic Systems

Multi-agent AI systems need exactly the guarantees described above. Each agent is an actor with isolated state: conversation history lives in one place, owned by one task, with no `Any` casting, and no state shared with other actors unless a message carries it (an `Arc<Mutex<_>>` in a message stays shared). Bounded mailboxes give token streams and API fan-out natural backpressure, so a slow consumer slows the producer instead of growing an unbounded queue. Tool calls map to type-safe request/response (`send`), and orchestration across multiple model APIs chains without callback hell. Flaky tool handlers become supervised children: a panic mid-call is a restart per strategy, not a silent death, and a crashed agent comes back with the same `ActorId` and configuration so the rest of the system can re-look it up and continue. None of this is a separate AI feature set. It is what failure handling traced to OTP buys you.

---

## Non-Goals

- **Remote messaging**: Tokio Actors is explicitly local (in-process)
- **Distributed systems**: tokio-actors is local-first and has no networking or clustering layer
- **Proc macros**: We keep it simple, just traits

---

## Architecture

Every actor's run loop is a dedicated `tokio::task` on the runtime you provide (a timer or stream registration runs as a further task there), sharing its worker threads with every other task: a callback that never yields holds its worker (see Blocking Work), and the crate adds no scheduler of its own.

A `biased; select!` gives every actor's run loop eight ordered branches: a dedicated **stop lane** for `stop()` (a per-actor cell with a version, not a channel - raising it never waits on mailbox capacity), then the **system channel** (status reads, `SupervisorRef` calls, `sys` calls and link signals), then the restart plane, the death plane, the verdict signal (an incarnation given the Unresponsive verdict), the forwarder plane, the **priority lane** (present only on an actor spawned with one), and finally the user mailbox; an actor that set a receive timeout or an idle threshold also reads that threshold last, after the mailbox found nothing. Each branch works even when the ones below it are full or backlogged. `status()` and `wait_stopped()` bypass all of them: they read a cell the runtime updates directly, so `status()` answers even if the actor is stuck processing a message and never touching `select!` at all, and `wait_stopped()` resolves as soon as the actor ends, an abort included.

---

## License

MIT OR Apache-2.0

---

**Built for Rust developers who value predictability over magic.**

For implementation details and edge cases, see [`examples/`](examples/) and [`tests/`](tests/).

---

## Author

**Saddam Uwejan** (Sam) - Rust systems engineer specializing in concurrent systems and production infrastructure.

- [LinkedIn](https://www.linkedin.com/in/uwejan/)

*Building production-ready Rust libraries for real-world problems.*
