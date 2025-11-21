# Tokio Actors

[![Crates.io](https://img.shields.io/crates/v/tokio-actors.svg)](https://crates.io/crates/tokio-actors)
[![Documentation](https://docs.rs/tokio-actors/badge.svg)](https://docs.rs/tokio-actors)
[![CI](https://github.com/uwejan/tokio-actors/workflows/CI/badge.svg)](https://github.com/uwejan/tokio-actors/actions)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

**Zero-ceremony, Tokio-native actors with strong typing and production-ready edge case handling.**

Tokio Actors is a lightweight actor framework built for Rust developers who want predictable concurrency without the complexity. Every actor runs as a dedicated `tokio::task` on your multi-threaded runtime—no custom schedulers, no hidden magic.

---

## ✨ Why Tokio Actors?

### 🎯 **Strongly Typed**
Message and response types are enforced at compile time. No runtime type casting, no `Any` trait abuse.

```rust
impl Actor for MyActor {
    type Message = MyMessage;   // ← Compile-time checked
    type Response = MyResponse;  // ← No guessing
}
```

### 🔒 **Bounded Mailboxes = Natural Backpressure**
Every actor has a bounded mailbox (default: 64). When full, senders wait automatically—no OOM crashes from runaway queues.

### ⏱️ **Timer Drift Handling (MissPolicy)**
Recurring timers have three drift strategies to handle system lag:
- **Skip**: Jump to next aligned tick
- **CatchUp**: Send all missed messages immediately  
- **Delay**: Reset timer from now

This is the kind of edge-case thinking production systems need.

### 🤖 **Perfect for AI/LLM Applications**
Actors naturally fit AI/LLM architectures:
- **Multi-Agent Systems**: Each LLM agent is an actor with isolated state
- **API Orchestration**: Coordinate multiple LLM API calls with backpressure
- **Conversation State**: Bounded mailboxes prevent memory bloat from chat history
- **Tool Calling**: Actors model tool execution with type-safe request/response
- **Async Workflows**: Chain LLM calls without callback hell

### 🚦 **Lifecycle Observability**
Query actor status anytime: `Initializing → Running → Stopping → Stopped`. Perfect for health checks and graceful degradation.

---

## 🚀 Quick Start

```bash
cargo add tokio-actors
```

### Ping-Pong: Request-Response Pattern

```rust
use async_trait::async_trait;
use tokio_actors::{
    actor::{Actor, ActorExt, context::ActorContext, handle::ActorHandle},
    ActorResult,
};

// Pong actor - simply responds to pings
#[derive(Default)]
struct PongActor {
    pings_received: u64,
}

#[derive(Clone)]
enum PongMsg {
    Ping,  // ← No manual reply_to needed!
}

#[derive(Clone)]
enum PongResp {
    Pong,  // ← Response goes through send() automatically
}

#[async_trait]
impl Actor for PongActor {
    type Message = PongMsg;
    type Response = PongResp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        let PongMsg::Ping = msg;
        self.pings_received += 1;
        Ok(PongResp::Pong)  // ← Just return the response!
    }
}

// Ping actor  
struct PingActor {
    pong: ActorHandle<PongActor>,
    pongs_received: u64,
}

#[derive(Clone)]
enum PingMsg {
    SendPing,
}

#[derive(Clone)]
enum PingResp {
    Ack,
}

#[async_trait]
impl Actor for PingActor {
    type Message = PingMsg;
    type Response = PingResp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            PingMsg::SendPing => {
                // Send Ping, get Pong back automatically through send()
                let resp = self.pong.send(PongMsg::Ping).await?;
                
                if matches!(resp, PongResp::Pong) {
                    self.pongs_received += 1;
                }
                Ok(PingResp::Ack)
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Spawn pong first
    let pong = PongActor::default().spawn_actor("pong", ()).await?;
    
    // Spawn ping with reference to pong
    let ping = PingActor { pong: pong.clone(), pongs_received: 0 }
        .spawn_actor("ping", ())
        .await?;

    // Send 10 pings - each gets automatic Pong response
    for _ in 0..10 {
        ping.send(PingMsg::SendPing).await?;
    }

    Ok(())
}
```

### Configuration is Completely Optional

```rust
// No config needed - uses defaults
actor.spawn_actor("my-actor", ()).await?;

// Or customize with builder pattern
let config = ActorConfig::default().with_mailbox_capacity(256);
actor.spawn_actor("my-actor", config).await?;

// Reference to config works too
actor.spawn_actor("my-actor", &config).await?;
```

---

## 🎭 Core Concepts

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
- `notify` errors → actor calls `handle_failure()` and **continues processing**
- `send` errors → actor stops (caller expects a response, failure is critical)

This asymmetry reflects real-world semantics.

### Timers with Drift Control

```rust
use tokio::time::Duration;
use tokio_actors::MissPolicy;

ctx.schedule_after(msg, Duration::from_secs(5))?;  // One-shot

ctx.schedule_recurring(
    msg,
    Duration::from_millis(100),
    MissPolicy::CatchUp,  // ← Send all missed ticks
)?;
```

**Edge Case**: Scheduling in the past? The message fires immediately. No panics, no silent failures.

### Lifecycle Hooks

```rust
async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    // Initialize state, schedule timers
    ctx.schedule_recurring(HealthCheck, Duration::from_secs(30), MissPolicy::Skip)?;
    Ok(())
}

async fn on_stopped(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
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
```

---

## 🧠 Deep Rust Patterns

### Why `Sync` is Required for Timer Factories

Recurring timers use closures that are held across `.await` points in a spawned task:

```rust
ctx.schedule_recurring_with(
    || generate_message(),  // ← Must be Sync
    Duration::from_secs(1),
    MissPolicy::Skip,
)?;
```

The closure lives in an `Arc` that's shared across tasks. Rust's `Send` future rules require this. For `schedule_recurring(msg, ...)` where `msg: Clone`, we require `msg: Sync` for the same reason—the closure `move || msg.clone()` captures `msg`.

**Workaround**: If your message isn't `Sync`, use `schedule_recurring_with` with a factory that doesn't capture state.

### ActorHandle Equality

Handles implement `PartialEq` based on `ActorId`, not channel identity:

```rust
let actor1 = MyActor.spawn_actor("foo", ()).await?;
let actor2 = actor1.clone();

assert_eq!(actor1, actor2);  // ✅ Same actor ID

let actor3 = MyActor.spawn_actor("bar", ()).await?;
assert_ne!(actor1, actor3);  // ✅ Different actor ID
```

This allows handles to be used in `HashSet` and `HashMap` for deduplication and routing.

### Bounded Mailbox Backpressure

When the mailbox is full:
- `notify().await` **blocks** until space is available
- `try_notify()` returns `TrySendError::Full` immediately
- `send().await` **blocks** (same as notify, just with response)

During timer catch-up (`MissPolicy::CatchUp`), we use `try_notify` to avoid blocking the timer task on a full mailbox. If the mailbox is full, we stop  the catch-up—better to skip than deadlock.

---

## 📊 API at a Glance

### ActorHandle Methods

| Method | Description |
|--------|-------------|
| `notify(msg)` | Fire-and-forget (awaits mailbox space) |
| `try_notify(msg)` | Non-blocking fire-and-forget |
| `send(msg)` | Request-response (awaits processing) |
| `stop(reason)` | Request actor to stop |
| `is_alive()` | Check if actor is still running |
| `mailbox_len()` | Current queue depth |
| `mailbox_available()` | Free space in mailbox |
| `id()` | Get actor ID |

### ActorContext Methods

| Method | Description |
|--------|-------------|
| `schedule_once(msg, when)` | Fire message at specific `Instant` |
| `schedule_after(msg, delay)` | Fire message after `Duration` |
| `schedule_recurring(msg, interval, policy)` | Recurring timer |
| `schedule_recurring_with(factory, interval, policy)` | Recurring with message factory |
| `cancel_timer(id)` | Cancel specific timer |
| `cancel_all_timers()` | Cancel all active timers |
| `active_timer_count()` | Number of active timers |
| `self_handle()` | Get handle to this actor |
| `status()` | Current lifecycle status |

### ActorConfig Builder

```rust
ActorConfig::default()
   .with_mailbox_capacity(512)
```

---

## 🧪 Testing

```bash
cargo test
```

Tests cover:
- Ping-pong bidirectional messaging
- Timer drift policies
- Mailbox backpressure
- Handle equality and hashing
- Lifecycle hooks
- Error propagation

---

## 📦 Examples

| Example | Description |
|---------|-------------|
| `simple_counter` | Basic notify/send usage |
| `ping_pong` | Bidirectional actor communication |
| `timers` | Recurring timers with MissPolicy |
| `cross_comm` | Multiple actors coordinating |

Run with:
```bash
cargo run --example ping_pong
```

---

## 🔮 Future Enhancements

### Planned
- **Supervision trees**: Declarative parent-child relationships
- **Actor registry**: Named global actor lookup
- **Graceful shutdown coordination**: Drain mailboxes before stopping
- **Telemetry hooks**: Metrics and tracing integration

### Non-Goals
- **Remote messaging**: Tokio Actors is explicitly local (in-process)
- **Distributed systems**: Use Akka/Orleans/Proto.Actor for that
- **Proc macros**: We keep it simple—just traits

---

## 🏗️ Architecture

Every actor is a dedicated `tokio::task`. No shared executor, no fancy scheduling—just Tokio doing what it does best.

---

## 📄 License

MIT OR Apache-2.0

---

**Built with ❤️ for Rust developers who value predictability over magic.**

For implementation details and edge cases, see [`examples/`](examples/) and [`tests/`](tests/).

---

## 👤 Author

**Saddam Uwejan** (Sam) - Rust systems engineer specializing in concurrent systems and production infrastructure.

- 🔗 [GitHub](https://github.com/uwejan)
- 💼 [LinkedIn](https://www.linkedin.com/in/uwejan/)
- 📧 saddamuwejan@gmail.com

*Building high-performance, production-ready Rust libraries for real-world problems.*
