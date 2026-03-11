# Tokio Actors

[![Crates.io](https://img.shields.io/crates/v/tokio-actors.svg)](https://crates.io/crates/tokio-actors)
[![Documentation](https://docs.rs/tokio-actors/badge.svg)](https://docs.rs/tokio-actors)
[![CI](https://github.com/uwejan/tokio-actors/workflows/CI/badge.svg)](https://github.com/uwejan/tokio-actors/actions)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

**Zero-ceremony, Tokio-native actors with strong typing and production-ready edge case handling.**

Tokio Actors is a lightweight actor framework built for Rust developers who want predictable concurrency without the complexity. Every actor runs as a dedicated `tokio::task` on your multi-threaded runtime -- no custom schedulers, no hidden magic.

---

## Why Tokio Actors?

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

### OTP-Style Supervision
Supervisors automatically restart failed children using restart strategies from Erlang/OTP:
- **OneForOne**: Restart only the failed child
- **OneForAll**: Restart all children when any one fails
- **RestForOne**: Restart the failed child and all children started after it
- **SimpleOneForOne**: Dynamic children sharing a single factory

Each child has a `RestartType` (Permanent/Transient/Temporary) and a sliding-window restart budget to prevent restart storms.

### Perfect for AI/LLM Applications
Actors naturally fit AI/LLM architectures:
- **Multi-Agent Systems**: Each LLM agent is an actor with isolated state
- **API Orchestration**: Coordinate multiple LLM API calls with backpressure
- **Conversation State**: Bounded mailboxes prevent memory bloat from chat history
- **Tool Calling**: Actors model tool execution with type-safe request/response
- **Async Workflows**: Chain LLM calls without callback hell

### Lifecycle Observability
Query actor status anytime via the system channel -- even when the mailbox is full:

```rust
let status = handle.get_status().await?;
println!("{}: {} children, {} timers", status.id, status.child_count, status.timer_count);
```

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

// Supervised parent (default: OneForOne, 3 restarts / 5s)
let h = my_actor.spawn().named("supervisor").supervised().await?;

// Supervised with custom strategy
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

This asymmetry reflects real-world semantics.

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

The runtime handles the restart loop: evaluate strategy, check budget, invoke the factory, wire the new child in -- all non-blocking. If the budget is exhausted, the supervisor itself stops.

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
    .supervised()                 // Optional: enable supervision (default config)
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
| `stop_child(id)` | Manually stop a supervised child |
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
    .supervised()                         // OneForOne, 3 restarts / 5s
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
