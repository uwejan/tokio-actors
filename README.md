# Tokio Actors

[![Crates.io](https://img.shields.io/crates/v/tokio-actors.svg)](https://crates.io/crates/tokio-actors)
[![Documentation](https://docs.rs/tokio-actors/badge.svg)](https://docs.rs/tokio-actors)
[![CI](https://github.com/uwejan/tokio-actors/workflows/CI/badge.svg)](https://github.com/uwejan/tokio-actors/actions)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

**The OTP-faithful actor runtime for Rust: supervision, lifecycle, and crash semantics traced to Erlang/OTP, running as zero-ceremony Tokio tasks.**

Tokio Actors is a lightweight actor framework for Rust developers who want Erlang-grade failure handling without the ceremony. Every actor runs as a dedicated `tokio::task` on your runtime -- no custom schedulers, no macros, no hidden magic. What sets it apart is what happens when things go wrong: supervision, restarts, and cleanup follow Erlang/OTP's documented semantics, not an approximation of them.

---

## Why OTP Fidelity Matters

The actor model's production value was never the mailbox API -- it is the failure semantics. Erlang/OTP's "let it crash" philosophy works because the runtime makes hard guarantees: every process death produces an exit signal that cannot be lost, supervisors stop and restart children in a documented order, cleanup (`terminate/2`) runs even when the process is dying from an exception, and a supervisor that exhausts its restart budget escalates with a reason its own parent knows how to interpret. Most Rust actor crates copy the API shape -- spawn, send, mailbox -- but not these failure semantics: panics slip past supervision, death notices ride best-effort channels, and restart logic is left as an exercise. Tokio Actors is built for OTP fidelity: every supervision and lifecycle semantic is traced to Erlang/OTP's documented behavior (`gen_server`, `supervisor`), and every deliberate deviation is documented as a deviation. When the docs here say a child restarts, that holds under a real crash -- a panicking handler, not just a polite `Err`.

## How It Compares

Three Rust actor frameworks, three different answers to "what happens when a handler panics?"

| Failure semantic | tokio-actors | ractor | Actix |
|---|---|---|---|
| Panic in a handler | Caught at the callback boundary; the actor stops with `StopReason::Failure(ActorError::Panic)` and `on_stopped` still runs with that reason (gen_server terminate-on-exception parity) | Caught once around the whole message loop; the payload is stringified into `ActorErr::Failed` | Not caught; the panic unwinds the task and the actor dies silently (senders find out via send errors) |
| Death notification | A runtime watcher on the child's task delivers `ChildStopped` through an awaited send -- the report cannot be silently dropped | The dying actor's own task notifies fire-and-forget over an unbounded port; the send result is discarded, so a dead receiver means a lost event | None; the actor's death is observable only through send errors |
| Automatic restart | Runtime-managed: OneForOne, OneForAll, RestForOne, SimpleOneForOne | Not in core ("supervision is presently left to the implementor"); the default supervisor callback stops itself | `Supervisor` restarts only after a graceful stop; a panic kills the supervisor task too |
| Group restart ordering | OTP ordering: affected children stop in reverse start order, then restart in start order | n/a | n/a |
| Manual child management | `terminate_child` / `restart_child` / `delete_child` / `stop_child` with OTP semantics: spec kept, manual stops never charge the budget | n/a | n/a |
| Restart budget escalation | Sliding-window budget; exhaustion stops the supervisor with the OTP `shutdown` reason (`StopReason::ParentRequest`), so a grandparent's Transient policy does not restart it | n/a | n/a |
| Forced termination | `Shutdown::Timeout` -> `Kill` -> task abort ladder; every stop path is bounded except documented `Infinity` | n/a | n/a |

*Competitor cells come from a source-level review of ractor v0.15.13 and Actix master (2026-07). n/a means no counterpart exists in that framework's supervision core (ractor leaves restart to the implementor; Actix restarts only after graceful stops). Credit where due: ractor's in-task catch is the right skeleton, and tokio-actors adopts a per-callback version of it -- the watcher layer and the restart machinery are where the designs diverge. The behaviors in the tokio-actors column are exercised by the test suite (see [Testing](#testing)).*

**Honest limits.** Panic capture requires unwinding: `panic = "abort"` builds have no in-process supervision (the process dies -- the same caveat ractor documents). A handler stuck at an `.await` point is force-killable, but a non-yielding busy loop is beyond even task abort; it surfaces as a typed `SupervisionError::ChildUnresponsive` after a bounded wait instead of hanging the supervisor. `Shutdown::Infinity` children can stall a group restart indefinitely, matching OTP's own infinity semantics. And there is no distribution layer by design: tokio-actors is local-first (see [Non-Goals](#non-goals)) -- in-process fidelity is the product, not a stepping stone to a cluster framework.

---

## Feature Highlights

### OTP-Style Supervision
Supervision is crash-visible. A panic in a handler or lifecycle hook is caught and becomes `StopReason::Failure(ActorError::Panic)`, and supervisors detect every child death through a runtime watcher on the child's task -- the death report cannot be silently dropped. Restart strategies come from Erlang/OTP:
- **OneForOne**: Restart only the failed child
- **OneForAll**: Restart all children when any one fails
- **RestForOne**: Restart the failed child and all children started after it
- **SimpleOneForOne**: Dynamic children sharing a single factory

Group strategies (OneForAll/RestForOne) follow OTP ordering: affected children are stopped in reverse start order, then restarted in start order. Children keep their `ActorId` and `ActorConfig` across restarts. Each child has a `RestartType` (Permanent/Transient/Temporary), and a sliding-window restart budget prevents restart storms. When the budget is exhausted, the supervisor stops with `StopReason::ParentRequest` -- its own supervisor's Transient policy will NOT restart it, matching OTP shutdown semantics.

### Lifecycle Observability
Query actor status anytime via the system channel -- even when the mailbox is full:

```rust
let status = handle.get_status().await?;
println!("{}: {} children, {} timers", status.id, status.child_count, status.timer_count);
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
Every actor has a bounded mailbox (default: 64). When full, senders wait automatically -- no OOM crashes from runaway queues.

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

// Anonymous (UUID auto-id)
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
})?;

// Named lookup (OTP whereis/1)
let handle = sys.get::<MyActor>("worker-1");

// Stop/kill by name
sys.stop("worker-1").await?;   // Graceful (vetoable)
sys.kill("worker-1").await?;   // Force (bypasses all hooks)

// Coordinated shutdown with timeout escalation
sys.shutdown().await;
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
        // Permanent -- always restarts, even after graceful stop
        ctx.spawn_child(|| Worker::default()).named("worker").await?;

        // Transient -- restarts on crash, stays down on graceful stop
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

The runtime handles the restart loop: evaluate strategy, check budget, invoke the factory, wire the new child in -- all non-blocking. If the budget is exhausted, the supervisor itself stops with `StopReason::ParentRequest`.

When a child must be stopped (group restart, parent shutdown, or a manual stop), its `Shutdown` policy bounds the whole exchange: `Shutdown::Timeout(d)` requests a cooperative stop, escalates to `Kill` at expiry, and backs the `Kill` with a task abort after a short grace window. A child stuck at an `.await` point IS killable -- abort cancels it at the next yield, and its drops still run. Only a handler spinning in a non-yielding busy loop is beyond reach; the supervisor waits a bounded time, then returns a typed `SupervisionError::ChildUnresponsive` instead of hanging.

#### Managing Children

Supervisors can also drive child lifecycles manually, mirroring OTP's `supervisor` module. Manual stops are not failures: they never charge the restart budget and never trigger sibling (OneForAll/RestForOne) restarts.

| Call | What it does |
|------|--------------|
| `ctx.terminate_child(id).await` | Stop WITHOUT restart (OTP `terminate_child`). The child spec is kept (unless Temporary) so the child can be revived later. Budget untouched. |
| `ctx.restart_child(id)` | Revive a terminated child from its stored spec -- same `ActorId`, same config (OTP `restart_child`). Errors with `ChildRunning`/`ChildRestarting` if the child is not down. |
| `ctx.delete_child(id)` | Remove the child spec (OTP `delete_child`). The child must not be running or restarting. |
| `ctx.stop_child(id).await` | Bounce per restart policy: Permanent children restart budget-free; Transient/Temporary stay down. |

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

Panics are crashes, not errors. A panic in `handle` or a lifecycle hook is caught at the callback boundary: the actor stops with `StopReason::Failure(ActorError::Panic)` (running `on_stopped` for post-init crashes, like gen_server's terminate-on-exception), and its supervisor (if any) restarts it per strategy. On the send path the caller sees the panic directly. `AskError` is flat in v0.7.0 (previously the transport error was nested as `Send(SendError)`):

```rust
use tokio_actors::{ActorError, AskError};

match worker.send(job).await {
    Ok(result) => println!("done: {result:?}"),
    Err(AskError::Actor(ActorError::Panic(msg))) => {
        // Handler panicked. The actor stopped with Failure(Panic) and its
        // supervisor is already restarting it. An immediate retry may hit
        // Closed during the restart window -- re-lookup by name to retry.
        eprintln!("worker crashed: {msg}");
    }
    Err(AskError::Actor(err)) => eprintln!("handler returned Err: {err}"),
    Err(AskError::Closed) => eprintln!("mailbox closed (actor down or mid-restart)"),
    Err(AskError::ResponseDropped) => eprintln!("actor stopped before replying"),
}
```

On the notify path, `Err` and panic are different animals: a handler that returns `Err` for a notify-dispatched message goes to `handle_failure()` and the actor **continues**; a handler that panics stops the actor and triggers a restart, exactly as on the send path.

**Known limitations**: `panic = "abort"` builds have no unwinding, so panic capture cannot exist there (the process dies). The default panic hook still prints to stderr even when supervision handles the crash -- set your own hook to silence it. A child with `Shutdown::Infinity` that refuses to stop can stall a group restart, matching OTP infinity semantics -- Infinity is the single unbounded case; every other stop path is bounded by the Timeout -> Kill -> abort ladder. A handler stuck at an `.await` point IS force-killable (the abort backstop cancels the task after a short grace); only a non-yielding busy loop remains beyond reach, and it surfaces as a typed `ChildUnresponsive` error after a bounded wait instead of hanging the supervisor.

### 3-Tier Termination

```rust
use tokio_actors::StopReason;

handle.stop(StopReason::Graceful).await?;   // Tier 1: pre_stop can veto
handle.stop(StopReason::Cancelled).await?;  // Tier 2: non-vetoable, on_stopped runs
handle.stop(StopReason::Kill).await?;       // Tier 3: bypasses ALL lifecycle hooks
```

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

// Non-blocking attempt (returns immediately)
handle.try_notify(msg)?;
```

**Error Handling Nuance**:
- `notify` errors -> actor calls `handle_failure()` and **continues processing**
- `send` errors -> actor stops (caller expects a response, failure is critical)
- Panics are not errors: a panic on **either** path stops the actor (see Crash Semantics above)

This asymmetry reflects real-world semantics.

### Timers with Drift Control

```rust
use tokio::time::Duration;
use tokio_actors::MissPolicy;

// One-shot after delay
ctx.schedule(msg).after(Duration::from_secs(5)).await?;

// Recurring -- default MissPolicy::Skip
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

---

## Deep Rust Patterns

### Why `Sync` is Required for Recurring Timer Messages

Recurring timers clone the message each tick via an internal `move || msg.clone()` closure held in an `Arc` across tasks. Rust's `Send` future rules require the captured `msg` to be `Sync`.

In practice this is a non-issue. Enum message types are `Sync` by default. Only types with unsynchronized interior mutability (`Cell`, `RefCell`) aren't `Sync`, and those also fail `Send`.

### ActorHandle Equality

Handles implement `PartialEq` based on `ActorId`, not channel identity:

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
    .await?;                      // Finalize: spawns the actor
```

### ActorHandle Methods

| Method | Description |
|--------|-------------|
| `notify(msg)` | Fire-and-forget (awaits mailbox space) |
| `try_notify(msg)` | Non-blocking fire-and-forget |
| `send(msg)` | Request-response (awaits processing) |
| `stop(reason)` | Stop via system channel (bypasses full mailbox) |
| `get_status()` | Introspection snapshot via system channel |
| `is_alive()` | Check if actor is still running |
| `mailbox_len()` | Current queue depth |
| `mailbox_available()` | Free space in mailbox |
| `mailbox_capacity()` | Total mailbox capacity |
| `id()` | Get actor ID |

### ActorContext Methods

| Method | Description |
|--------|-------------|
| `spawn_child(factory)` | Returns a [`ChildSpawnBuilder`] - chain `.named()`, `.restart_type()`, `.shutdown()`, `.with_config()` |
| `children()` | Introspection info for all supervised children |
| `terminate_child(id)` | Stop a child WITHOUT restart (spec kept for later revival) |
| `restart_child(id)` | Revive a terminated child from its stored spec |
| `delete_child(id)` | Remove a terminated child's spec |
| `stop_child(id)` | Stop a child, restarting per policy (budget-free bounce) |
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
| `kill(name)` | Force kill by name |
| `shutdown()` | Coordinated shutdown with escalation |
| `registered()` | List all registered actor names |

### ActorConfig Builder

```rust
ActorConfig::default()
    .with_mailbox_capacity(512)
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

Multi-agent AI systems need exactly the guarantees described above. Each agent is an actor with isolated state: conversation history lives in one place, owned by one task, with no shared mutable state and no `Any` casting. Bounded mailboxes give token streams and API fan-out natural backpressure, so a slow consumer slows the producer instead of growing an unbounded queue. Tool calls map to type-safe request/response (`send`), and orchestration across multiple model APIs chains without callback hell. Flaky tool handlers become supervised children: a panic mid-call is a restart per strategy, not a silent death, and a crashed agent comes back with the same `ActorId` and configuration so the rest of the system can re-look it up and continue. None of this is a separate AI feature set -- it is what OTP fidelity buys you.

---

## Future Enhancements

### Planned
- **Telemetry hooks**: Metrics and tracing integration
- **Priority messages**: Typed channel abstraction mapping to OTP EEP 76

### Non-Goals
- **Remote messaging**: Tokio Actors is explicitly local (in-process)
- **Distributed systems**: Use Akka/Orleans/Proto.Actor for that
- **Proc macros**: We keep it simple, just traits

---

## Architecture

Every actor is a dedicated `tokio::task`. No shared executor, no fancy scheduling, just Tokio doing what it does best.

Stop signals and status queries flow through a dedicated **system channel** with `biased; select!` priority over the user mailbox. This means `stop()` and `get_status()` work even when the mailbox is full.

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
