# Tokio Actors

[![Crates.io](https://img.shields.io/crates/v/tokio-actors.svg)](https://crates.io/crates/tokio-actors)
[![Documentation](https://docs.rs/tokio-actors/badge.svg)](https://docs.rs/tokio-actors)
[![CI](https://github.com/uwejan/tokio-actors/workflows/CI/badge.svg)](https://github.com/uwejan/tokio-actors/actions)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust Version](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)

**The OTP-faithful actor runtime for Rust: supervision, lifecycle, and crash semantics traced to Erlang/OTP, running as zero-ceremony Tokio tasks.**

Tokio Actors is a lightweight actor framework for Rust developers who want Erlang-grade failure handling without the ceremony. Every actor runs as a dedicated `tokio::task` on your runtime. No custom schedulers, no macros, no hidden magic. What sets it apart is what happens when things go wrong: supervision, restarts, and cleanup follow Erlang/OTP's documented semantics, not an approximation of them.

---

## Why OTP Fidelity Matters

The actor model's production value was never the mailbox API. It is the failure semantics. Erlang/OTP's "let it crash" philosophy works because the runtime makes hard guarantees: every process death produces an exit signal that cannot be lost, supervisors stop and restart children in a documented order, cleanup (`terminate/2`) runs even when the process is dying from an exception, and a supervisor that exhausts its restart budget escalates with a reason its own parent knows how to interpret. Most actor libraries copy the API shape - spawn, send, mailbox. Tokio Actors is built for the failure semantics: every supervision and lifecycle behavior is traced to Erlang/OTP's documented behavior (`gen_server`, `supervisor`), and every deliberate deviation is documented as a deviation. When the docs here say a child restarts, that holds under a real crash - a panicking handler, not just a polite `Err`.

## The Guarantees

What happens when things go wrong. Every row is exercised by the test suite (see [Testing](#testing)):

| Failure semantic | tokio-actors guarantee |
|---|---|
| Panic in a handler | Caught at the callback boundary; the actor stops with `StopReason::Failure(ActorError::Panic)` and `on_stopped` still runs with that reason (gen_server terminate-on-exception parity) |
| Death notification | A runtime runner on the child's task records its fate, and the supervisor's run loop delivers a `ChildEvent` to `on_child_stopped` for every child death that ends before the supervisor's message loop ends; a death still pending when the supervisor itself stops for any reason other than `Kill` is delivered once before `on_stopped` with `SupervisionAction::SupervisorStopping`, and one stopped with `Kill` delivers none. A child the supervisor's own teardown stops is not reported to `on_child_stopped`; with the `tracing` feature its end is logged as a warning when it is not orderly |
| Automatic restart | Runtime-managed: OneForOne, OneForAll, RestForOne, SimpleOneForOne |
| Group restart ordering | A group restart stops its members one at a time in reverse start order, each ending, or being given the Unresponsive verdict, before the next is signalled, and initiates restarts in start order, each after the previous member re-registered (OTP ordering); a member whose stop was already in progress keeps stopping beside them, and a `terminate_child` of a member the group has not yet reached, or a `stop_child` of a `Transient` or `Temporary` one, stops that member at once, beside the member the group is stopping, which an OTP supervisor never does. A SimpleOneForOne supervisor stops its children concurrently, each under its own `Shutdown` policy. |
| Sequential restart init completion | A group restart awaits each member's full init handshake before initiating the next: sequential, not atomic. A mid-group init failure aborts the remaining starts and retries the group, charged once to the restart budget: a `OneForAll` retry stops and restarts every child, a child started during the group included, and a `RestForOne` retry the failed member and every child after it, so the members started before the failed one keep the incarnation the first attempt gave them (OTP parity). A member released by `terminate_child` stays down through every retry, a departure listed below. The supervisor keeps answering its own messages throughout |
| Manual child management | `terminate_child` / `restart_child` / `delete_child` / `stop_child` modeled on OTP's `supervisor` calls (answers that differ are listed under the deliberate departures below): spec kept, manual stops never charge the budget and complete under the child's `Shutdown` even if the caller abandons the wait. `terminate_child` succeeds for any known child, one already down or whose restart decision is still owed included, and cancels a pending restart, except that a child whose restart attempt is in flight answers `ChildRestarting`, and an actor of its subtree that never yields makes it answer `ChildUnresponsive` with the child left down; it returns only once the child's whole subtree has ended, a child already stopped whose actors are still ending included. `stop_child` returns `StopChildOutcome::Absorbed` when a pending group restart has still to stop and restart that `Permanent` child, a retry that will restart it again included, and `ChildRestarting` once that group has restarted it for good or released it, or while its own restart attempt is in flight; it stops a `Transient` or `Temporary` member of a pending group restart as `terminate_child` does, returning `StopChildOutcome::Stopped`. A `terminate_child` made while a group restart is pending keeps that child down through every retry of that group. A same-name `spawn_child` after `delete_child`, or after a `Temporary` or pool child was removed, waits until the previous child's subtree has ended. A failed `restart_child` is reported through `on_child_stopped` with `Removed` and never charges the budget |
| Restart budget escalation | Sliding-window budget; exhaustion stops the supervisor with the OTP `shutdown` reason (`StopReason::ParentRequest`), so a grandparent's Transient policy does not restart it |
| Forced termination | `Shutdown::Timeout` -> `Kill` -> task abort ladder; every stop path is bounded except a documented `Infinity`, a bare `stop()` (its `pre_stop` may keep vetoing, and once it is accepted nothing escalates it, so its `on_stopped`, final deliveries and child teardown run without a bound; `stop_within` is its bounded form), and a child still initializing under `Infinity` without a `start_timeout` |
| Enumeration & kill | `actor_ids()`, `actor_status(id)`, and `kill_by_id(id)` reach every actor registered in a system, named or anonymous - enumeration parity with `erlang:processes/0`, untrappable kill parity with `erlang:exit(Pid, kill)`; a `Kill` from `kill(name)`, `kill_by_id` or `stop(StopReason::Kill)` that the target accepts while it is registered ends in the abort backstop, after the target's kill grace (`kill_grace` on its spawn builder, 120 ms by default, zero aborting at once) |

**Pairing rule.** A parent's `Shutdown` covers its children only when it is at least the sum of their stop budgets (each child's `Shutdown`, plus its kill grace, plus the short wait that confirms an abort), or the largest of them for a `SimpleOneForOne` pool. A root supervisor stopped by `ActorSystem::shutdown` gets `per_actor_timeout` (5 s by default) for its whole teardown. A shorter window escalates the parent to `Kill`, and its children still running then take the path of a killed supervisor's children: workers receive `Kill`, and a child that is itself a supervisor stops in order within its own `Shutdown`. Erlang/OTP states a stronger rule for supervisor children: "If the child process is another supervisor, the shutdown time must be set to infinity to give the subtree ample time to shut down." (`supervisor` documentation, child specification).

**Limits.** Panic capture requires unwinding: `panic = "abort"` builds have no in-process supervision (the process dies). A handler stuck at an `.await` point is force-killable, but a non-yielding busy loop is beyond even task abort; it surfaces as a typed `SupervisionError::ChildUnresponsive` after a bounded wait instead of hanging the supervisor: from a manual stop, and as the stop reason of a supervisor whose pending group restart can only go on through it. With the `tracing` feature, each verdict logs one warning when it is made, once per incarnation whichever route reaches it: the child's stop ladder, or a restart attempt's, `terminate_child`'s or `spawn_child`'s wait for the subtree that incarnation left running. Where a call waits for the child, the same verdict also reaches it as `ChildUnresponsive`, and where none does (a restart attempt, or a supervisor's own teardown) the child is not restarted while it lives. A restart attempt whose wait for the previous incarnation's subtree expires instead stops its supervisor, with that `ChildUnresponsive` as its failure, so the supervisor's own parent decides. A bare `stop()` can be vetoed by `pre_stop` indefinitely, and once it is accepted nothing escalates it; `stop_within` is the bounded form. A child still initializing under `Shutdown::Infinity` without a `start_timeout` is waited on without a bound. `Shutdown::Infinity` children can stall a group restart indefinitely, matching OTP's own infinity semantics. A child factory that never returns blocks its supervisor, as an OTP start function that never returns blocks its supervisor. And there is no distribution layer by design: tokio-actors is local-first (see [Non-Goals](#non-goals)). In-process fidelity is the product, not a stepping stone to a cluster framework.

---

## Lifecycle Contracts

Every lifecycle promise is a mechanism the runtime enforces, not a documentation convention:

| Contract | Guarantee |
|---|---|
| Truthful spawn | `spawn().await` acks through `pre_start` AND `on_started` before it resolves, with an infinity default and an opt-in `.start_timeout(dur)`. A failed init returns `SpawnError::Init` instead of dying silently, or `SpawnError::Killed` when a `Kill` reaches the actor before its end is final; `.detached()` provides fire-and-forget spawning for callers who want it. |
| Uniform crash semantics | `Err` from `handle` stops the actor the same way whether the message arrived via `send` or `notify` - one crash path, not two. Recoverable conditions belong in the `Response` type as `Ok` values; crash cleanup belongs in `on_stopped`, which receives the `StopReason`. |
| Observable lifecycle | `ActorHandle::status()` and `wait_stopped()` form a `watch`-based runtime plane that answers even when an actor is stuck at an `.await` point; `get_status()` serves the richer queue-plane snapshot. |
| Bounded, truthful shutdown | `ActorSystem::shutdown()` awaits every root actor to a terminal state (stopping roots only, in reverse registration order, so each supervisor tears down its own subtree) and returns a `ShutdownReport`: `outcomes` for the roots, `swept` for any non-root leftover the final sweep handles (normally empty): each leftover is waited on, bounded by the global deadline (the call's `ShutdownPolicy::timeout`, or the earlier deadline of a sweep that started in a call that has not completed, a dropped call included, which the system holds until the last shutdown in flight completes), and the ones still running then are force-stopped; after the deadline a shutdown can run on for the forced roots' longest kill grace plus the abort settle time, then the forced leftovers' longest kill grace plus the abort settle time, and that time once more plus the abort settle time for a leftover whose abort, scheduled with the system, is issued late. A leftover reads `Graceful` when its end became final before the deadline, `Killed` when it became final at or after it or when the shutdown's force took a `Kill` on it, `Aborted` when its task ended without publishing `Stopped`, and `Unresponsive` when even the abort did not end it within the settle time after it. It needs a Tokio runtime with the time driver enabled. Do not call it from inside an actor that is part of the shutdown being awaited: that call hangs. |
| Matchable errors | `ActorError` carries structured `Timer`/`Stream`/`Supervision` variants instead of stringified messages, `send_timeout` spans one call-wide deadline (`AskError::Timeout { enqueued }`: `enqueued: false` means the request never reached the mailbox and is safe to retry; `true` means the deadline raced the reply, retry only if idempotent), and every error enum is exhaustive. |

In the same spirit: one-shot schedules are lazy and `#[must_use]` (a dropped builder never fires), `.every_with(factory)` covers recurring messages that are not `Clone`, and `ActorSystem::get_by_id` resolves an `ActorId` to a live handle.

---

## Liveness Model

An actor runs until it is stopped, killed, or its system shuts down; every actor is enumerable and killable through its system. This mirrors Erlang/OTP's mechanism set: links and monitors deliver every death as a signal that cannot be lost, and `supervisor` trees are the documented way to structure a system's lifetime (erlang.org/docs/28). Structuring actors under supervision trees is the recommended way to manage their lifetimes here for the same reason.

Internal self-references - a recurring timer schedule, a stream forwarder, `self_handle()` used inside a callback - never extend an actor's lifetime, and observers (status watchers, `wait_stopped()` futures) never root one either: process lifetime governs timer lifetime, not the reverse (Erlang/OTP's `timer` BIFs cancel a pid-addressed timer automatically when its target dies; erlang.org/docs/28).

The deliberate departures from Erlang/OTP, each documented rather than silently assumed:
- Initial sibling spawns during a supervisor's startup may overlap; only restarts are strictly sequential.
- A restarted child's init wait is unbounded by default, matching OTP; `spawn_child(..).start_timeout(dur)` opts a specific child into a bound instead.
- A supervisor keeps processing its own mailbox throughout a group restart; OTP's `supervisor` process is blocked for the whole operation.
- A stop can be vetoed by `pre_stop`. The veto is bounded by the Timeout -> Kill -> abort ladder except under `Shutdown::Infinity`, and a bare `stop()` that `pre_stop` keeps vetoing is unbounded, with `stop_within` its bounded form, where Erlang/OTP's `terminate/2` cannot refuse a stop.
- `Kill` is observed at the next turn boundary, so the `Kill` itself never interrupts a callback already begun; a callback still awaiting once the target's kill grace has passed is aborted at that `.await` by the abort backstop, where an Erlang/OTP kill acts anywhere in the execution flow.
- Every `Kill` accepted while its target is registered ends in an abort backstop after the target's own kill grace (120 ms unless its spawn builder sets another, zero allowed), and a loop that never yields survives it, where Erlang/OTP needs no backstop because a kill is certain. A target that has left the registry is in the last step of its end, waiting only for its timer and stream tasks to finish, and is not aborted; one that has sealed its stop lane refuses the `Kill`.
- Stop requests coalesce on one lane and are ranked inside a strength, where Erlang/OTP delivers every signal in per-sender order.
- A supervisor's loop never blocks on a start, restart or group stop that the supervisor makes itself, where Erlang/OTP's supervisor is blocked through each; only a handler holds it, by awaiting a manual stop (for `terminate_child`, the child's whole subtree included) or a same-name `spawn_child` that waits for a previous child's subtree, until that wait ends.
- Supervision is configuration of an actor, not a separate process, where an Erlang/OTP supervisor is a dedicated `gen_server`.
- `restart_child` returns before the child's init completes, and a failed manual start is reported through `on_child_stopped`, where Erlang/OTP's call is synchronous and returns the start error.
- A `spawn_child` made while a group restart is pending starts at once, and its init may overlap the group, where Erlang/OTP serves `start_child` after the restart, in the same process.
- Supervisor children default to `Timeout(5 s)` like any other child, governed by the pairing rule above, where Erlang/OTP defaults a supervisor child's shutdown to `infinity`, the value its documentation asks for.
- System shutdown signals roots only, each supervisor stopping its own subtree, with a per-root bound (5 s by default) and a global deadline (30 s by default) at which every actor still running is forced; past that deadline the call can run on for the forced actors' kill graces and the short wait that confirms an abort, where the Erlang/OTP application master waits without a bound.
- System shutdown (`shutdown` and `shutdown_with`) and `stop_within` are driven by the caller's future, where in Erlang/OTP `init` owns node shutdown and `sys:terminate/3` bounds only the caller's wait. A dropped future takes no further step, but every `Kill` it raised keeps its scheduled abort, and once a shutdown's final sweep has begun the system holds that sweep's deadline `Kill` and abort.
- A child that never yields gets one `Unresponsive` verdict, which escalates to its supervisor when the child blocks progress, and a group restart's one-at-a-time stop passes over a member given the verdict, so the next member is signalled while the unresponsive one may still run, and a member whose stop was already in progress keeps stopping beside them. Erlang/OTP waits for the `'DOWN'` after a kill without limit, so it stops a group's members one after another and never signals the next beside a live one. Such a child keeps its name while it runs, and can make each ancestor in turn stop itself.
- A restart, a `terminate_child` and a same-name `spawn_child` wait for the previous child's subtree to end, bounded by the stop budgets that subtree carries, except where a supervisor child with `Infinity` lies in it, whose orderly stop under a killed parent has no bound; a `terminate_child` of a child already down waits for its old subtree the same way. Erlang/OTP has no such wait and avoids the race with `infinity`, the shutdown its documentation asks for on a supervisor child.
- A killed supervisor's supervisor children tear down in order within their own `Shutdown`, then take `Kill` and, once their own kill grace has passed, an abort (under `Shutdown::Infinity` nothing bounds that teardown, unless a `Kill` already stands on the child, which keeps its abort), where an Erlang/OTP child supervisor that traps exits runs its `terminate/2` without a bound under `infinity`. The `ParentKilled` stop that begins this ends a `Transient` child cleanly, so it is not restarted, as for `ParentRequest`, where an Erlang/OTP `transient` child is restarted on any exit reason other than `normal`, `shutdown` and `{shutdown, Term}`, so a killed end is abnormal for it.
- A worker takes its parent's orderly stop like a trapping process (`pre_stop` and `on_stopped` run) but receives `Kill` when its parent is killed, where a non-trapping Erlang/OTP worker dies at once on any non-normal parent exit.
- Pool children (`SimpleOneForOne`) are each stopped under their own `Shutdown`, where Erlang/OTP uses one timer from the pool's single shared spec.
- The default restart budget is 3 restarts in 5 s, where Erlang/OTP's default intensity is 1 in 5 s. A zero `restart_window` is accepted, where Erlang/OTP rejects a zero period at start: a restart counts while it is no older than the window, inclusive, so a zero window exhausts the budget only when restarts read the same instant (for example on a paused clock that does not move between them), and a `max_restarts` of 0 exhausts it at the first restart the strategy would make.
- When a supervisor stops for any reason but `Kill`, each child death still pending when its message loop ended is delivered once to `on_child_stopped` with `SupervisionAction::SupervisorStopping` before `on_stopped` (no restart, no budget charge, `ctx.status()` reading `Stopping`), and supervision calls made from such a delivery are refused with `SupervisorStopping`; a `Kill` that arrives meanwhile ends the rest. These deliveries spend the supervisor's own `Shutdown` window (a root's `stop_within` window or the system shutdown's per-root timeout) and have no bound under `Shutdown::Infinity` or a bare `stop()`. A `Kill` delivers none, and an end during the teardown that follows has no callback. This goes beyond Erlang/OTP, whose supervisor delivers no exit to a callback once its termination began and only logs such an end as a `shutdown_error` report.
- A stop is taken at the next turn boundary, ahead of any child report or message queued before it, so a child's end that arrived first is never evaluated before the stop, where an Erlang/OTP supervisor, trapping exits, handles that child's exit first.
- `is_alive()` reads false from the moment the actor's message loop is done (the return of `on_stopped`, or the loop's end under a `Kill`, which skips that hook), through the teardown of the actor's children, while `status()` reads `Stopping` until `Stopped` (an actor whose loop ended with `Kill` keeps `Running` until `Stopped`), where Erlang/OTP's `is_process_alive/1` stays true until `terminate/2` returns. The mailbox stays open while `on_stopped` runs, so a message sent then is accepted and then dropped unhandled; a send after `on_stopped` returns is refused.
- A `terminate_child`, or a `stop_child` of a non-`Permanent` member or of a non-`Permanent` child started since, made while a group restart is pending stops at once a member (or such a child) that the group's one-at-a-time stops have not reached, so two children may stop at once, and keeps that child down through every retry of that group. An Erlang/OTP supervisor serves `terminate_child` before or after one restart attempt, never between its one-at-a-time stops, and its own retry starts every child of the list again, except the member whose start failed, whose termination cancels the retry.
- A `stop_child` of a `Permanent` member of a pending group restart whose restart by that group is still ahead, a retry that will restart it again after another member failed to start included, returns at once as absorbed, so the call adds no incarnation to the ones the group gives the member, where Erlang/OTP's serial order would restart the member once more, unless it is the member whose start failed, whose termination between two attempts cancels the retry.
- During a pending group restart, a `stop_child` of a `Permanent` member that the group has restarted for good or released (a retry that will restart the member again absorbs the call instead), a `restart_child` of any member of the group (a released one included) and a `delete_child` of an awaited or queued member answer `ChildRestarting` until the group restart completes, a call made between two attempts of the group's retry included. An Erlang/OTP supervisor serves such a call between the attempts, because its retry is a message the supervisor sends itself: there the bounce's equivalent, a `terminate_child` then a `restart_child`, gives the member one more incarnation, and a `restart_child` or `delete_child` of a member already running again is answered `{error, running}`, except that for the member whose start failed Erlang/OTP also answers `{error, restarting}` and its termination cancels the retry. The caller can retry once the group completes.
- `restart_child` and `delete_child` answer `ChildRestarting` while the child's stop is in progress and while its group restart is pending (for `restart_child`, a member released by `terminate_child` included; `delete_child` answers such a member by its own state), where OTP's supervisor, which serves one call at a time, would not see those states.
- A child that died and whose restart decision is still owed (a crash a bounce met, or a death the supervisor has not handled yet) is answered `ChildRestarting` by `stop_child`, `restart_child` and `delete_child`: the supervisor makes that decision once and these calls neither make it nor replace it, where an Erlang/OTP supervisor answers `restart_child` and `delete_child` with `{error, running}` until it has processed the exit and has no bounce call. `terminate_child` of such a child dismisses the decision, keeps the child down, reports its death once to `on_child_stopped` as `Removed`, and returns once the child's whole subtree has ended, as OTP's `terminate_child` flushes the exit and answers `ok`.
- A spawn whose initialization failed and that a `Kill` then reached before its end was final reads `SpawnError::Killed`, the one answer its status plane and every observer also give, where an Erlang/OTP starter receives the initialization's error while its monitors see the kill.

---

## Feature Highlights

### OTP-Style Supervision
Supervision is crash-visible. A panic in a handler or lifecycle hook is caught and becomes `StopReason::Failure(ActorError::Panic)`. Supervisors detect every child death through a runtime runner on the child's task, and the run loop delivers a `ChildEvent` to `on_child_stopped` for every death that ends before the supervisor's message loop ends (a child the supervisor's own teardown stops is not reported there). A death still pending when the supervisor itself stops for any reason other than `Kill` is delivered once before `on_stopped` with `SupervisionAction::SupervisorStopping`, and one stopped with `Kill` delivers none. Restart strategies come from Erlang/OTP:
- **OneForOne**: Restart only the failed child
- **OneForAll**: Restart all children when any one fails
- **RestForOne**: Restart the failed child and all children started after it
- **SimpleOneForOne**: Dynamic children sharing a single factory

Group strategies (OneForAll/RestForOne) follow OTP ordering: affected children are stopped in reverse start order, then restarted in start order (a `terminate_child` of a member the group has not yet reached, or a `stop_child` of a `Transient` or `Temporary` one, stops that member beside the one the group is stopping, a departure listed above). Children keep their `ActorId` and `ActorConfig` across restarts. Each child has a `RestartType` (Permanent/Transient/Temporary), and a sliding-window restart budget prevents restart storms. When the budget is exhausted, the supervisor stops with `StopReason::ParentRequest`, and its own supervisor's Transient policy will NOT restart it, matching OTP shutdown semantics.

### Lifecycle Observability
Two observability planes, matching OTP's `sys:get_status` vs `process_info(Pid, status)` split. The system channel gives a rich snapshot even when the mailbox is full:

```rust
let status = handle.get_status().await?;
println!("{}: {} children, {} timers", status.id, status.child_count, status.timer_count);
```

The runtime plane answers instantly, even for an actor stuck at an `.await` point inside `handle`, and gives you an awaitable terminal state:

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
}
```

### Bounded Mailboxes = Natural Backpressure
Every actor has a bounded mailbox (default: 64). When full, senders wait automatically - no OOM crashes from runaway queues.

### Timer Drift Handling (MissPolicy)
Recurring timers have three drift strategies to handle system lag:
- **Skip**: Jump to next aligned tick
- **CatchUp**: Send all missed messages immediately
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
use tokio_actors::{actor::ActorExt, ActorConfig, ActorSystem, SupervisionConfig};

// Anonymous (UUID auto-id, registers as a root in the default system)
let h = my_actor.spawn().await?;

// Named (registered in default system)
let h = my_actor.spawn().named("worker-1").await?;

// Named with custom mailbox
let config = ActorConfig::default().with_mailbox_capacity(256);
let h = my_actor.spawn().named("worker-1").with_config(config).await?;

// On a specific system
let sys = ActorSystem::create("my-system")?;
let h = my_actor.spawn().named("worker-1").on_system(&sys).await?;

// Supervisor (children restart via OneForOne, 3 restarts / 5s)
let h = my_actor.spawn().named("supervisor").supervisor().await?;

// Supervisor with custom strategy
let sup = SupervisionConfig::one_for_all().max_restarts(10, Duration::from_secs(60));
let h = my_actor.spawn().named("supervisor").with_supervision(sup).await?;
```

### Actor Registry (ActorSystem)

```rust
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

// Named lookup (OTP whereis/1)
let handle = sys.get::<MyActor>("worker-1");

// Stop/kill by name
sys.stop("worker-1").await?;   // Graceful (vetoable)
sys.kill("worker-1").await?;   // Force (bypasses all hooks)

// Every actor, named or anonymous, is enumerable and killable by id
let ids = sys.actor_ids();
let status = sys.actor_status(&ids[0]);
let report = sys.kill_by_id(&ids[0]).await;   // Option<StopReport>: None when the id is not registered or its actor refuses the Kill; otherwise the target takes the same Kill, kill grace and abort as with kill(name), and this call also waits for its end (a short settle wait after an abort) to report it

// Coordinated shutdown: stops root actors only (each supervisor tears down
// its own subtree), awaits every root to a terminal state, and reports
let report = sys.shutdown().await;
```

---

## Core Concepts

### Supervision

Supervisors spawn children through their `ActorContext` and automatically handle restarts:

```rust
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
#   async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
}

// Launch with OneForAll strategy and custom budget
let sup = SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(30));
let handle = MySupervisor.spawn().named("my-sup").with_supervision(sup).await?;
```

The runtime handles the restart loop: evaluate strategy, check budget, invoke the factory, wire the new child in, all non-blocking. If the budget is exhausted, the supervisor itself stops with `StopReason::ParentRequest`.

When a child must be stopped (group restart, parent shutdown, or a manual stop), its `Shutdown` policy bounds the whole exchange: `Shutdown::Timeout(d)` requests a cooperative stop, escalates to `Kill` at expiry, and backs the `Kill` with a task abort once the child's kill grace has passed (`kill_grace` on its spawn builder, 120 ms by default). A child stuck at an `.await` point IS killable - abort cancels it at the next yield, and its drops still run. Only a handler spinning in a non-yielding busy loop is beyond reach; the supervisor waits a bounded time, then returns a typed `SupervisionError::ChildUnresponsive` to a manual stop, or stops with it when a pending group restart waits for the child, instead of hanging. With the `tracing` feature, each verdict logs one warning when it is made, once per incarnation whichever route reaches it: the child's stop ladder, or a restart attempt's, `terminate_child`'s or `spawn_child`'s wait for the subtree that incarnation left running. Where a call waits for the child, the same verdict also reaches it as `ChildUnresponsive`, and where none does (a restart attempt, or a supervisor's own teardown) the child is not restarted while it lives. A restart attempt whose wait for the previous incarnation's subtree expires instead stops its supervisor, with that `ChildUnresponsive` as its failure, so the supervisor's own parent decides.

**Kill grace.** Every actor has a kill grace: the time it is given, after a `Kill` is raised on it, to reach its next turn boundary before its task is aborted. It is 120 ms unless the spawn builder sets another (`.kill_grace(dur)` on `spawn()` for a root and on `spawn_child(..)` for a child), and a child keeps it across every restart. `Duration::ZERO` aborts the task as soon as the `Kill` is raised, so a callback in progress is cut at its next `.await` and never observes the `Kill` itself. The grace applies to every kill of the actor: `kill(name)`, `kill_by_id`, `stop(StopReason::Kill)`, the kill step of `stop_within` and of system shutdown, its supervisor's stop of it, and the kill a worker receives when its own supervisor is killed. An actor that is itself a supervisor stops in order when its supervisor is killed, bounded by its own `Shutdown`, and the grace applies to the `Kill` raised once that window has passed. `stop_within(StopReason::Kill, within)` waits its own `within` instead, for that call. After the abort, the drops of the actor's timers and streams must finish within the short wait that confirms it: a `Drop` that takes longer makes the actor read as one that does not yield, so a restart waiting for it ends with `ChildUnresponsive` and `kill_by_id` reports `Unresponsive`. The system schedules the abort when the `Kill` is accepted while the actor is still registered, so dropping the call that raised it never cancels the abort. Through an actor's handle (`stop`, `stop_within`) a `Kill` is accepted until the actor seals its stop lane, just before its queued messages and its value are dropped, and after that it answers `SendError::Closed`; that includes the time it runs `on_stopped` and stops its children, and every other reason is refused with `SendError::Closed` from the moment its message loop ends; `ActorSystem::stop` and `kill` answer `SendError::NotFound` once the actor has left the registry, an aborted one included.

#### Managing Children

Supervisors can also drive child lifecycles manually, mirroring OTP's `supervisor` module. Manual stops are not failures: they never charge the restart budget and never trigger sibling (OneForAll/RestForOne) restarts.

| Call | What it does |
|------|--------------|
| `ctx.terminate_child(id).await` | Stop WITHOUT restart (OTP `terminate_child`). The child spec is kept (unless Temporary) so the child can be revived later. Budget untouched. Succeeds for any known child, one already down or whose restart decision is still owed included: it dismisses the owed decision, cancels a pending restart, and returns once the child's whole subtree has ended. Errors with `ChildRestarting` only for a child whose restart attempt is in flight, and with `ChildUnresponsive` when an actor of the subtree never yields. |
| `ctx.restart_child(id)` | Revive a terminated child from its stored spec - same `ActorId`, same config (OTP `restart_child`). Errors with `ChildRunning` while the child's task runs, and with `ChildRestarting` while a restart attempt is in flight, the child's stop is in progress, its death awaits its restart decision, or its group restart is pending (a member released by `terminate_child` included). |
| `ctx.delete_child(id)` | Remove the child spec (OTP `delete_child`). Errors as `restart_child` does, except that a member released by `terminate_child` from a pending group restart is answered by its own state. |
| `ctx.stop_child(id).await` | Bounce per restart policy: Permanent children restart budget-free; Transient/Temporary stay down. Returns `StopChildOutcome::Absorbed` for a `Permanent` member of a pending group restart that has still to stop and restart it, a retry that will restart it again included, and `ChildRestarting` once that restart has restarted it for good or released it, or while the member's own restart attempt is in flight; a `Transient` or `Temporary` member of a pending group restart is stopped as by `terminate_child` and returns `StopChildOutcome::Stopped`. Errors with `ChildRestarting` for a child whose death awaits its restart decision. |

To force a restart, kill the child and let its restart policy restart it: call `ActorSystem::kill_by_id` with the child's id, or `stop_within(StopReason::Kill, d)` on its handle. An `ActorContext` has no kill by id, so a supervisor handler gets the system with `ActorSystem::get_named`, passing the name the system was created with, and calls `kill_by_id` on it, or uses a handle it kept. A Permanent child, and a Transient child (a Kill is an abnormal exit), is then restarted by its supervisor and the restart is charged to the budget, as Erlang/OTP does for `exit(Child, kill)`; a Temporary child is not restarted. `stop_child` is the budget-free bounce.

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

### Crash Semantics

Panics are crashes, not errors. A panic in `handle` or a lifecycle hook is caught at the callback boundary: the actor stops with `StopReason::Failure(ActorError::Panic)` (running `on_stopped` for post-init crashes, like gen_server's terminate-on-exception), and its supervisor (if any) restarts it per strategy. On the send path the caller sees the panic directly. `AskError` is flat - transport, actor, and timeout failures are peer variants - and carries `Timeout { enqueued }` from `send_timeout`:

```rust
use tokio_actors::{ActorError, AskError};

match worker.send(job).await {
    Ok(result) => println!("done: {result:?}"),
    Err(AskError::Actor(ActorError::Panic(msg))) => {
        // Handler panicked. The actor stopped with Failure(Panic) and its
        // supervisor is already restarting it. An immediate retry may hit
        // Closed during the restart window - re-lookup by name to retry.
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
}
```

On the notify path, `Err` and a panic behave identically to the send path: both stop the actor with `StopReason::Failure(..)`, `on_stopped` runs, and a supervisor (if any) restarts it per strategy. A notify caller never learns about the crash directly (fire-and-forget has no return channel), while a send caller sees it as `Err(AskError::Actor(..))`. Recoverable outcomes are `Ok`-encoded values in `Response`.

**Known limitations**: `panic = "abort"` builds have no unwinding, so panic capture cannot exist there (the process dies); see `PANIC_CAPTURE_AVAILABLE`. The default panic hook still prints to stderr even when supervision handles the crash. Set your own hook to silence it. A child with `Shutdown::Infinity` that refuses to stop can stall a group restart, matching OTP infinity semantics. The unbounded stop cases are a child under `Shutdown::Infinity`, a bare `stop()` (on a handle, or `ActorSystem::stop` by name), which `pre_stop` may keep vetoing and whose stop, once accepted, nothing escalates (`stop_within` is its bounded form), and a child still initializing under `Infinity` without a `start_timeout`; every other stop path is bounded by the Timeout -> Kill -> abort ladder. A handler stuck at an `.await` point IS force-killable (the abort backstop cancels the task once its kill grace has passed); only a non-yielding busy loop remains beyond reach, and it surfaces as a typed `ChildUnresponsive` error after a bounded wait instead of hanging the supervisor (from a manual stop, or as the stop reason of a supervisor whose pending group restart waits for it). With the `tracing` feature, each verdict logs one warning when it is made, once per incarnation whichever route reaches it: the child's stop ladder, or a restart attempt's, `terminate_child`'s or `spawn_child`'s wait for the subtree that incarnation left running. Where a call waits for the child, the same verdict also reaches it as `ChildUnresponsive`, and where none does (a restart attempt, or a supervisor's own teardown) the child is not restarted while it lives. A restart attempt whose wait for the previous incarnation's subtree expires instead stops its supervisor, with that `ChildUnresponsive` as its failure, so the supervisor's own parent decides.

Further limits of the stop and restart paths:
- A `Kill` sent during `on_stopped` is observed when that hook returns, and a hook still awaiting once the actor's kill grace has passed (120 ms by default) is aborted at its next `.await`; the final reason is `Kill` either way.
- A supervisor's stop can take one extra child window when a child was restarting.
- An `Infinity` group member blocks the members queued behind it.
- An initialization that waits on a dependency being restarted can stall under `Infinity` unless `start_timeout` is set.
- A restart after a supervisor's `Kill` waits for the subtree its previous incarnation left, bounded by the stop budgets that subtree carries, except as the next bullet says.
- A supervisor child with `Infinity` whose own supervisor is killed stops in order without a bound, so the restart of the killed supervisor waits for it without one until that supervisor child has ended (then for the rest of the subtree, bounded by its stop budgets). So does a `terminate_child` of any child whose subtree holds such a supervisor, and so does a same-name `spawn_child` after `delete_child` or a removal while such a subtree is still ending: each of these holds the calling handler, and with it the supervisor's message loop, until that subtree ends.
- A stop ordered while a restarted child's acknowledgment waits for its supervisor is first signalled when the supervisor adopts it, after the supervisor's current callback, and `terminate_child` answers `ChildRestarting` while a restart attempt is in flight.
- A supervisor whose `pre_start` or `on_started` fails never ran its message loop and receives no `on_child_stopped` (with the `tracing` feature, a child that ended with an unexpected reason during that initialization is logged as a warning).
- An error or panic in a final delivery makes the supervisor's exit reason `Failure` once `on_stopped` has returned, while the remaining deliveries still run.
- A start cut short by its own `start_timeout` reads `Kill` from `wait_stopped()`, its supervisor's report carries the start timeout as its failure, and the caller of an awaited spawn reads `SpawnError::StartTimeout`.

### 3-Tier Termination

```rust
use tokio_actors::StopReason;

handle.stop(StopReason::Graceful).await?;   // Tier 1: pre_stop can veto
handle.stop(StopReason::Cancelled).await?;  // Tier 2: non-vetoable, on_stopped runs
handle.stop(StopReason::Kill).await?;       // Tier 3: bypasses ALL lifecycle hooks
```

### Bounded Stop

`stop()` raises a stop signal and returns immediately; nothing bounds how long `pre_stop` may veto it, nor, once it is accepted, how long the actor's `on_stopped` and the stop of its children take. `stop_within` bounds the wait and reports how far the escalation ladder had to go:

```rust
use tokio_actors::{StopOutcome, StopReason};

match handle.stop_within(StopReason::Graceful, Duration::from_secs(5)).await? {
    StopOutcome::Graceful => println!("stopped without this call's Kill taking effect"),
    StopOutcome::Killed => println!("stopped after this call's Kill was taken"),
    StopOutcome::Aborted => println!("its task ended without publishing Stopped"),
    StopOutcome::Unresponsive => println!("no terminal status within the call's last bound"),
}
```

This stops this incarnation only. A supervised actor's supervisor may restart it per its own policy the moment it reaches a terminal status; to stop a supervised child for good, use `ctx.terminate_child(id)` from the parent instead.

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
- The only difference left is what the CALLER learns: `notify` cannot report a crash back (fire-and-forget has no return channel), while `send`/`send_timeout` surface it as `Err(AskError::Actor(err))`.

The messaging method you pick changes what the caller learns, never what the actor does.

### Priority Lane

A priority lane is a second, independent mailbox that four `*_priority` handle methods send on, received ahead of the normal mailbox. There is no default: enable it with an explicit capacity.

```rust
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

A priority message is received before every normal message still queued, including a normal message the same sender sent earlier; no message, priority or not, interrupts a callback already in progress. On an actor spawned without a priority lane, all four methods behave exactly like their normal-lane siblings, and `ActorStatusInfo` reports the lane's length and capacity separately from the normal mailbox's.

**Ordering.** The run loop receives, highest first: the stop lane, the system channel, the supervision branches (restart plane, death plane, the signal of an Unresponsive verdict, forwarders), the priority lane, and the normal mailbox. Within a lane messages keep their send order, and a stop request never travels on the priority lane.

**Starvation.** The lane's capacity bounds its memory and parks a sender that fills it, but nothing gives the normal mailbox a share: a priority flood at or above the rate the actor consumes it starves the normal mailbox for as long as it lasts, though no message is lost and the normal mailbox drains once the flood ends. A sustained storm of supervision events, which sit above the lane, delays the lane and the normal mailbox alike. Size the lane small and keep its traffic rare.

Erlang/OTP's own guidance applies: "You very seldom need to resort to usage of priority messages. Receiving processes have not been optimized for handling large amounts of priority messages." (Erlang Reference Manual, Processes)

### Timers with Drift Control

```rust
use tokio::time::Duration;
use tokio_actors::MissPolicy;

// One-shot after delay
ctx.schedule(msg).after(Duration::from_secs(5)).await?;

// Recurring - default MissPolicy::Skip
ctx.schedule(msg).every(Duration::from_millis(100)).await?;

// Recurring with explicit drift strategy
ctx.schedule(msg).every(Duration::from_millis(100))
    .on_miss(MissPolicy::CatchUp)
    .await?;
```

**Edge Case**: Scheduling in the past? The message fires immediately. No panics, no silent failures.

### Mailbox Monitoring

```rust
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

Off by default. Turn it on to see how long each actor's current callback (`pre_start`, `on_started`, `handle`, `pre_stop`, `on_child_stopped` or `on_stopped`) has been running:

```rust
use tokio_actors::{ActorSystem, SystemConfig};

let config = SystemConfig::default().with_turn_warning_threshold(Duration::from_secs(1));
let sys = ActorSystem::create_with("game", config)?;

// Or, on any system, at any time:
sys.set_turn_warning_threshold(Some(Duration::from_secs(1)));

sys.turn_elapsed(&ids[0]);  // Some(d) whenever that actor is inside a callback and a threshold is set
sys.stuck_actors();         // every actor over the threshold, longest first
```

This is observation only: it never interrupts a callback or changes how a stop resolves, and `actor_status` still reports the status the actor holds (`Initializing`, `Running` or `Stopping`) for a wedged actor - `turn_elapsed` and `stuck_actors` are how to tell. While no threshold is set, callbacks are not timed: `turn_elapsed` returns `None` for every actor and `stuck_actors` is empty. While one is set, each callback costs one extra monotonic clock read. With the `tracing` feature, a callback that exceeds the threshold is also logged once as a warning.

### Blocking Work

A `handle` invocation runs to completion before the actor observes anything else, so blocking inside it blocks that actor, and on a busy runtime it can starve its neighbors. Three tools cover it, all from the runtime you already own. For bounded CPU or blocking work, move it off the turn: `tokio::task::spawn_blocking(move || work()).await`. For a blocking call that needs `&mut self`, wrap it in `tokio::task::block_in_place(|| self.conn.execute(..))` (multi-threaded runtimes only). For an actor that is always busy, give it its own small runtime: an actor runs on whichever runtime is current when `.spawn().await` is called, so spawning it from inside a second runtime isolates it completely while abort, timers, and supervision keep working. To detect a wedged actor without any setup, probe it: `tokio::time::timeout(limit, handle.get_status())` times out when the actor has been inside one turn for longer than `limit`. A handler that never yields cannot be force-stopped by any of these; it surfaces as `ChildUnresponsive`.

```rust
// Inside any actor:
async fn handle(&mut self, msg: Job, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    tokio::task::spawn_blocking(move || run_job(msg))
        .await
        .map_err(|e| ActorError::user(e.to_string()))?;
    Ok(())
}
```

---

## Deep Rust Patterns

### Recurring Timer Messages: `Clone`, Not `Sync`

Recurring timers do not require `A::Message: Sync`. The per-tick factory is a `Box<dyn FnMut() -> Message + Send>` owned by exactly one forwarder task, so `Sync` is never demanded. `.every(msg)` needs `A::Message: Clone` (the factory clones `msg` each tick); `.every_with(|| build_message())` drops the `Clone` bound entirely by calling your factory closure instead. One-shot schedules (`.at()`, `.after()`) need only `Send` - no `Clone`, no `Sync`.

### ActorHandle Equality

Handles implement `PartialEq` based on the `ActorId` and the owning system's name, not channel identity:

```rust
let actor1 = MyActor.spawn().named("foo").await?;
let actor2 = actor1.clone();

assert_eq!(actor1, actor2);  // Same actor ID

let actor3 = MyActor.spawn().named("bar").await?;
assert_ne!(actor1, actor3);  // Different actor ID
```

This allows handles to be used in `HashSet` and `HashMap` for deduplication and routing.

### Bounded Mailbox Backpressure

When the mailbox is full:
- `notify().await` **blocks** until space is available
- `try_notify()` returns `TrySendError::Full` immediately
- `send().await` **blocks** (same as notify, just with response)

During timer catch-up (`MissPolicy::CatchUp`), we use `try_notify` to avoid blocking the timer task on a full mailbox. If the mailbox is full, we stop the catch-up. Better to skip than deadlock.

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
    .detached()                   // Optional: skip the init ack entirely
    .await?;                      // Finalize: awaits pre_start + on_started, then spawns
```

`.start_timeout(dur)` and `.detached()` are alternatives: pick a bound on the init ack or skip the ack, not both.

### ActorHandle Methods

| Method | Description |
|--------|-------------|
| `notify(msg)` | Fire-and-forget (awaits mailbox space) |
| `try_notify(msg)` | Non-blocking fire-and-forget |
| `send(msg)` | Request-response (awaits processing) |
| `send_timeout(msg, dur)` | Request-response bounded by one deadline spanning enqueue + response |
| `stop(reason)` | Stop via the stop lane (bypasses the system channel and a full mailbox alike) |
| `stop_within(reason, within)` | Stop bounded by `within`; reports `StopOutcome` (Graceful/Killed/Aborted/Unresponsive) |
| `get_status()` | Rich introspection snapshot via system channel (queue plane) |
| `status()` | Instant lifecycle phase from the runtime plane (answers even for a hung actor) |
| `wait_stopped()` | Awaits the actor's terminal state and returns its stop reason |
| `is_alive()` | Check if actor is still running: false from the moment its message loop is done (the return of `on_stopped`, or the loop's end under a `Kill`), through the teardown of the actor's children, while `status()` reads `Stopping` until `Stopped` (`Running` until `Stopped` when a `Kill` ended the loop) |
| `mailbox_len()` | Current queue depth |
| `mailbox_available()` | Free space in mailbox |
| `mailbox_capacity()` | Total mailbox capacity |
| `notify_priority(msg)` | Fire-and-forget on the priority lane (degrades to `notify` without one) |
| `try_notify_priority(msg)` | Non-blocking fire-and-forget on the priority lane |
| `send_priority(msg)` | Request-response on the priority lane |
| `send_priority_timeout(msg, dur)` | Request-response on the priority lane bounded by one deadline |
| `priority_len()` | Current priority lane queue depth (0 without a lane) |
| `priority_capacity()` | Total priority lane capacity (0 without a lane) |
| `id()` | Get actor ID |

### ActorContext Methods

| Method | Description |
|--------|-------------|
| `spawn_child(factory)` | Returns a [`ChildSpawnBuilder`] - chain `.named()`, `.restart_type()`, `.shutdown()`, `.kill_grace()`, `.with_config()`, `.start_timeout()` |
| `children()` | Introspection info for all supervised children |
| `terminate_child(id)` | Stop a child WITHOUT restart (spec kept for later revival) |
| `restart_child(id)` | Revive a terminated child from its stored spec |
| `delete_child(id)` | Remove a terminated child's spec |
| `stop_child(id)` | Stop a child, restarting per policy (budget-free bounce); returns `StopChildOutcome::Absorbed` for a `Permanent` member of a pending group restart that has still to restart it, `ChildRestarting` once that restart has restarted it for good or released it, and stops a non-`Permanent` member as `terminate_child` does |
| `schedule(msg)` | Returns a [`ScheduleBuilder`] - chain `.at(instant)`, `.after(delay)`, or `.every(interval)` |
| `cancel_timer(id)` | Cancel specific timer |
| `cancel_all_timers()` | Cancel all active timers |
| `active_timer_count()` | Number of active timers |
| `add_stream(stream)` | Attach an external stream to the mailbox |
| `cancel_stream(id)` | Cancel a specific stream |
| `cancel_all_streams()` | Cancel all active streams |
| `active_stream_count()` | Number of active streams |
| `self_handle()` | Get handle to this actor |
| `actor_id()` | This actor's ID |
| `actor_name()` | This actor's registered name |
| `status()` | Current lifecycle status |

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
| `kill_by_id(id)` | Force kill by id: the target takes the same `Kill`, kill grace and abort as with `kill(name)`, and this call also waits for its end, with a short settle wait after an abort; returns `Option<StopReport>`, `None` if the id is not registered or its actor refuses the `Kill` |
| `shutdown()` | Roots-only coordinated shutdown; awaits every root to a terminal state and returns a `ShutdownReport` (`outcomes` for roots, `swept` for the non-root leftovers of the final sweep, each waited on, bounded by the global deadline the system holds until the last shutdown in flight completes, and force-stopped if still running then; the time driver is required) |
| `registered()` | List all registered actor names |
| `set_turn_warning_threshold(threshold)` | Turns the turn watchdog on (`Some(d)`) or off (`None`); observation only |
| `turn_warning_threshold()` | Current turn watchdog threshold, if any |
| `turn_elapsed(id)` | How long that actor's current callback has run; `None` between callbacks or with the watchdog off |
| `stuck_actors()` | Actors over the threshold, longest first; empty with the watchdog off |

### ActorConfig Builder

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
- Lifecycle hooks and 3-tier termination (Kill bypass)
- ActorSystem registry, spawn methods, and shutdown
- Supervision strategies, restart budget, child lifecycle
- Panic capture, crash restarts, and group restart ordering
- Stream integration (add_stream, StreamEvent, cancellation)
- SpawnBuilder chain (all combinations)
- Error propagation and type preservation

---

## Examples

| Example | Description |
|---------|-------------|
| `simple_counter` | Basic notify/send usage |
| `ping_pong` | Bidirectional actor communication |
| `timers` | Recurring timers with MissPolicy |
| `cross_comm` | Multiple actors coordinating |
| `stream_counter` | External stream integration |
| `supervision` | Parent-child supervision with restart |

Run with:
```bash
cargo run --example supervision
```

---

## A Foundation for Agentic Systems

Multi-agent AI systems need exactly the guarantees described above. Each agent is an actor with isolated state: conversation history lives in one place, owned by one task, with no shared mutable state and no `Any` casting. Bounded mailboxes give token streams and API fan-out natural backpressure, so a slow consumer slows the producer instead of growing an unbounded queue. Tool calls map to type-safe request/response (`send`), and orchestration across multiple model APIs chains without callback hell. Flaky tool handlers become supervised children: a panic mid-call is a restart per strategy, not a silent death, and a crashed agent comes back with the same `ActorId` and configuration so the rest of the system can re-look it up and continue. None of this is a separate AI feature set. It is what OTP fidelity buys you.

---

## Future Enhancements

### Planned
- **Telemetry hooks**: Metrics and tracing integration

### Non-Goals
- **Remote messaging**: Tokio Actors is explicitly local (in-process)
- **Distributed systems**: tokio-actors is local-first and has no networking or clustering layer
- **Proc macros**: We keep it simple, just traits

---

## Architecture

Every actor is a dedicated `tokio::task`. No shared executor, no fancy scheduling, just Tokio doing what it does best.

A `biased; select!` gives every actor's run loop seven ordered branches: a dedicated **stop lane** for `stop()` (a `watch` signal, not a channel - it never waits on mailbox capacity), then the **system channel** for `get_status()`, then the restart plane, the death plane, the forwarder plane, the **priority lane** (present only on an actor spawned with one), and finally the user mailbox. Each branch works even when the ones below it are full or backlogged. `status()` and `wait_stopped()` bypass all of them: they read a `watch` value the runtime updates directly, so they answer even if the actor is stuck processing a message and never touching `select!` at all.

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

*Building high-performance, production-ready Rust libraries for real-world problems.*
