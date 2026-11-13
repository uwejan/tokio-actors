//! A test mirror of every Rust block in README.md, a check that the mirror
//! still carries each block line for line, and a test for each row of its
//! Patterns table.
//!
//! Each block test compiles and runs the block's code as the README writes
//! it. What a block leaves implicit (an actor type, a handle, a message
//! value) is declared inside the test, and a line added only to make a menu
//! of alternatives run as one program is marked `setup:`. The README's
//! `println!`, `eprintln!`, `warn!` and `error!` lines are captured into a
//! per-thread buffer so a test can assert which branch a block took.
//!
//! Every test is named after its README section and carries the heading it
//! mirrors, so an edit to a block shows which test to update.

// The README blocks bind values they never read, as examples do.
#![allow(unused_variables)]

mod common;

// The check that keeps this file and README.md in step. It reads the README
// and this file at compile time, takes every fenced `rust` block out of the
// README, and requires that
//
// - the README holds exactly `README_RUST_BLOCKS` such blocks, so a block
//   added or removed fails the count until a mirror is added or dropped;
// - every non-blank line of every block appears in this file's mirror
//   section (the part after the marker line below), in the README's order,
//   comparing text with all whitespace removed and with a comma before a
//   closing bracket ignored, so formatting and line wrapping do not matter
//   but a changed word, name, number or comment does.
//
// A few README lines are spelled another way in the mirror, because the mirror
// has to compile (an import list in the order rustfmt writes it, a call chain
// with `.on_system(&sys)` added, a match arm with braces). Each such line is
// listed in `SPELLED_DIFFERENTLY` with the spelling the mirror uses. The list
// sits above the marker, so it cannot satisfy the check by itself, and an
// entry whose README line no longer exists fails the check.

const README: &str = include_str!("../README.md");
const SOURCE: &str = include_str!("readme.rs");
const README_RUST_BLOCKS: usize = 22;
const MIRROR_MARKER: &str = "// ----- the mirrored blocks and their stubs follow -----";

/// README lines paired with the spelling the mirror uses for them.
const SPELLED_DIFFERENTLY: &[(&str, &str)] = &[
    (
        "use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult, StopReason};",
        "use tokio_actors::{actor::{context::ActorContext, Actor, ActorExt}, ActorResult, StopReason};",
    ),
    (
        "    actor::{Actor, ActorExt, context::ActorContext},",
        "    actor::{context::ActorContext, Actor, ActorExt},",
    ),
    (
        "let handle = MySupervisor.spawn().named(\"my-sup\").with_supervision(sup).await?;",
        "let handle = MySupervisor.spawn().named(\"my-sup\").with_supervision(sup).on_system(&sys).await?;",
    ),
    (
        "    StopOutcome::Unresponsive => println!(\"no terminal status within the call's last bound\"),",
        "StopOutcome::Unresponsive => { println!(\"no terminal status within the call's last bound\") }",
    ),
    (
        "let sys = ActorSystem::create_with(\"game\", config)?;",
        "let sys = ActorSystem::create_with(\"watchdog-game\", config)?;",
    ),
    (
        "        .max_restarts(10, Duration::from_secs(60)))  // Custom strategy + budget",
        ".max_restarts(10, Duration::from_secs(60)));  // Custom strategy + budget",
    ),
];

/// Removes all whitespace and every comma that sits just before a closing
/// bracket, so rustfmt's layout and trailing commas compare equal.
fn squash(text: &str) -> String {
    let chars: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    let mut out = String::with_capacity(chars.len());
    for (i, &c) in chars.iter().enumerate() {
        let before_closer = matches!(chars.get(i + 1), Some(')' | ']' | '}'));
        if c == ',' && before_closer {
            continue;
        }
        out.push(c);
    }
    out
}

/// The comparison key of one README line: squashed, without a trailing comma
/// (the comma may belong before a closing bracket on the next line).
fn line_key(line: &str) -> String {
    squash(line).trim_end_matches(',').to_string()
}

/// The lines of each fenced `rust` block of `readme`, in order.
fn rust_blocks(readme: &str) -> Vec<Vec<String>> {
    let text = readme.replace("\r\n", "\n");
    let mut blocks = Vec::new();
    let mut open: Option<Vec<String>> = None;
    for line in text.lines() {
        match open.as_mut() {
            None => {
                if line.starts_with("```rust") {
                    open = Some(Vec::new());
                }
            }
            Some(body) => {
                if line.starts_with("```") {
                    blocks.extend(open.take());
                } else {
                    body.push(line.to_string());
                }
            }
        }
    }
    assert!(open.is_none(), "a rust block in README.md is never closed");
    blocks
}

/// The README lines, one string per problem, that the mirror `source` does
/// not carry, plus a count problem when the block count is not `expected`.
fn mirror_problems(readme: &str, source: &str, expected: usize) -> Vec<String> {
    let blocks = rust_blocks(readme);
    let mut problems = Vec::new();
    if blocks.len() != expected {
        problems.push(format!(
            "README.md has {} rust blocks, this file mirrors {}",
            blocks.len(),
            expected
        ));
    }
    let source = source.replace("\r\n", "\n");
    let mut after_marker = source.lines().skip_while(|l| l.trim() != MIRROR_MARKER);
    assert!(
        after_marker.next().is_some(),
        "the marker line is missing from this file"
    );
    let mirror = squash(&after_marker.collect::<Vec<_>>().join("\n"));

    let mut used = vec![false; SPELLED_DIFFERENTLY.len()];
    let mut from = 0;
    for (n, block) in blocks.iter().enumerate() {
        for line in block {
            let key = line_key(line);
            if key.is_empty() {
                continue;
            }
            let spelled = SPELLED_DIFFERENTLY
                .iter()
                .position(|(readme_line, _)| line_key(readme_line) == key);
            let wanted = match spelled {
                Some(i) => {
                    used[i] = true;
                    line_key(SPELLED_DIFFERENTLY[i].1)
                }
                None => key,
            };
            match mirror[from..].find(&wanted) {
                Some(at) => from += at + wanted.len(),
                None => problems.push(format!(
                    "block {} (from 1): no mirror line, in order, for: {}",
                    n + 1,
                    line.trim()
                )),
            }
        }
    }
    for (i, (readme_line, _)) in SPELLED_DIFFERENTLY.iter().enumerate() {
        if !used[i] {
            problems.push(format!(
                "a listed spelling matches no README line: {readme_line}"
            ));
        }
    }
    problems
}

/// Every rust block of README.md has its mirror here, line for line.
#[test]
fn every_readme_rust_block_has_a_mirror() {
    let problems = mirror_problems(README, SOURCE, README_RUST_BLOCKS);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

// ----- the mirrored blocks and their stubs follow -----

use std::cell::RefCell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, Notify};

type TestResult = Result<(), Box<dyn std::error::Error>>;

thread_local! {
    static OUTPUT: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn record(line: String) {
    OUTPUT.with(|out| out.borrow_mut().push(line));
}

fn take_output() -> Vec<String> {
    OUTPUT.with(|out| std::mem::take(&mut *out.borrow_mut()))
}

macro_rules! println {
    ($($t:tt)*) => { record(format!($($t)*)) };
}
macro_rules! eprintln {
    ($($t:tt)*) => { record(format!($($t)*)) };
}
macro_rules! warn {
    ($($t:tt)*) => { record(format!($($t)*)) };
}
macro_rules! error {
    ($($t:tt)*) => { record(format!($($t)*)) };
}

/// Runs a README block on the flavor the environment selects.
fn run_block<F>(body: F)
where
    F: Future<Output = TestResult>,
{
    common::run(0, async move {
        take_output();
        body.await.expect("the README block returned an error");
    });
}

/// Runs a README block on a paused clock, for a block whose outcome depends
/// on a window or a grace: the clock moves only when the runtime is idle.
fn run_paused_block<F>(body: F)
where
    F: Future<Output = TestResult>,
{
    common::run_paused(async move {
        take_output();
        body.await.expect("the README block returned an error");
    });
}

/// The blocks that spawn into the default system under a fixed name run one
/// at a time.
static DEFAULT_SYSTEM: Mutex<()> = Mutex::new(());

fn default_system_lock() -> MutexGuard<'static, ()> {
    DEFAULT_SYSTEM.lock().unwrap_or_else(|e| e.into_inner())
}

/// Waits, by observation, until `condition` holds. The bound only limits how long a real hang takes to report.
async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let waited = tokio::time::timeout(Duration::from_secs(60), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "never observed: {what}");
}

/// Waits until `name` is free in `sys`.
async fn name_free(sys: &tokio_actors::ActorSystem, name: &str) {
    wait_until("a name to leave the registry", || {
        !sys.registered().iter().any(|n| n == name)
    })
    .await;
}

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorStatus, ActorSystem, AskError,
    Shutdown, SpawnError, StopOutcome, StopReason, StreamEvent, SupervisionConfig, TrySendError,
};

/// The `my_actor` / `MyActor` of the README: a unit actor that echoes.
#[derive(Clone, Copy, Debug, Default)]
struct MyActor;

impl Actor for MyActor {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, msg: u32, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        Ok(msg)
    }
}

/// An actor that adds what it is sent and answers the running total.
#[derive(Debug, Default)]
struct Summer(u32);

impl Actor for Summer {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, msg: u32, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        self.0 += msg;
        Ok(self.0)
    }
}

// ---------------------------------------------------------------------------
// Lifecycle Observability
// ---------------------------------------------------------------------------

/// README: "### Lifecycle Observability", first block (`get_status`).
#[test]
fn lifecycle_observability_get_status() {
    run_block(async {
        let sys = ActorSystem::create("readme-observability-status")?;
        // setup: a handle.
        let handle = MyActor.spawn().on_system(&sys).await?;

        let status = handle.get_status().await?;
        println!(
            "{}: {} children, {} timers",
            status.id, status.child_count, status.timer_count
        );

        let lines = take_output();
        assert_eq!(
            lines,
            vec![format!("{}: 0 children, 0 timers", handle.id())]
        );
        handle.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

/// README: "### Lifecycle Observability", second block (`status`,
/// `wait_stopped`).
#[test]
fn lifecycle_observability_runtime_plane() {
    run_block(async {
        let sys = ActorSystem::create("readme-observability-plane")?;
        // setup: a handle, stopped by a task once the status has been read, so
        // the awaitable terminal state resolves.
        let handle = MyActor.spawn().on_system(&sys).await?;
        let (read, status_read) = oneshot::channel::<()>();
        let stopper = handle.clone();
        tokio::spawn(async move {
            let _ = status_read.await;
            stopper.stop(StopReason::Graceful).await.unwrap();
        });

        let phase = handle.status(); // ActorStatus, no channel round trip
        read.send(()).unwrap(); // setup
        handle.wait_stopped().await; // resolves with the actor's stop reason once it reaches Stopped

        assert_eq!(phase, ActorStatus::Running);
        assert_eq!(handle.status(), ActorStatus::Stopped);
        assert_eq!(handle.wait_stopped().await, StopReason::Graceful);
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Strongly Typed
// ---------------------------------------------------------------------------

/// README: "### Strongly Typed".
#[test]
fn strongly_typed() {
    run_block(async {
        struct MyMessage;
        struct MyResponse;
        struct MyActor;

        impl Actor for MyActor {
            type Message = MyMessage; // Compile-time checked
            type Response = MyResponse; // No guessing

            // ...
            async fn handle(
                &mut self,
                msg: MyMessage,
                _ctx: &mut ActorContext<Self>,
            ) -> ActorResult<MyResponse> {
                todo!()
            }
        }

        let handle = MyActor
            .spawn()
            .on_system(&ActorSystem::create("readme-typed")?)
            .await?;
        // The block's `handle` is a placeholder: the types are what it pins.
        assert!(matches!(
            handle.send(MyMessage).await,
            Err(AskError::Actor(ActorError::Panic(_)))
        ));
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Quick Start
// ---------------------------------------------------------------------------

/// README: "### Counter: The Basics", run exactly as written, with its own
/// `#[tokio::main]` runtime.
#[test]
fn quick_start_counter_the_basics_as_written() {
    let _lock = default_system_lock();
    use tokio_actors::{
        actor::{context::ActorContext, Actor, ActorExt},
        ActorResult, StopReason,
    };

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
        counter.notify(5).await?; // fire-and-forget
        let total = counter.send(3).await?; // request-response -> 8
        counter.stop(StopReason::Graceful).await?;
        Ok(())
    }

    main().expect("the README main returned an error");
}

/// README: "### Counter: The Basics", the values its comments state (8 after
/// a notify of 5 and a send of 3), on both test flavors.
#[test]
fn quick_start_counter_the_basics_values() {
    run_block(async {
        #[derive(Default)]
        struct Counter(i64);

        impl Actor for Counter {
            type Message = i64;
            type Response = i64;

            async fn handle(
                &mut self,
                msg: i64,
                _ctx: &mut ActorContext<Self>,
            ) -> ActorResult<i64> {
                self.0 += msg;
                Ok(self.0)
            }
        }

        let sys = ActorSystem::create("readme-counter")?;
        let counter = Counter::default()
            .spawn()
            .named("counter")
            .on_system(&sys)
            .await?;
        counter.notify(5).await?;
        let total = counter.send(3).await?;
        assert_eq!(total, 8);
        counter.stop(StopReason::Graceful).await?;
        assert_eq!(counter.wait_stopped().await, StopReason::Graceful);
        Ok(())
    });
}

/// README: "### Spawning Actors". The block lists alternatives that reuse the
/// names `worker-1` and `supervisor`, so each alternative runs after the
/// previous actor has left the registry (setup lines).
#[test]
#[allow(clippy::clone_on_copy)]
fn quick_start_spawning_actors() {
    let _lock = default_system_lock();
    run_block(async {
        use std::time::Duration;
        use tokio_actors::{actor::ActorExt, ActorConfig, ActorSystem, SupervisionConfig};

        // setup: the actor value.
        let my_actor = MyActor;
        let default_system = ActorSystem::default();
        name_free(&default_system, "worker-1").await;

        // Each spawn below is an alternative. `spawn()` takes the actor by value, so `my_actor` is cloned,
        // and a name is free again when its actor has stopped (`wait_stopped()` has returned).

        // Anonymous (UUID auto-id, registers as a root in the default system)
        let h = my_actor.clone().spawn().await?;
        assert!(default_system.actor_ids().contains(h.id()));
        // setup: leave the registry before the next alternative.
        h.stop(StopReason::Graceful).await?;
        h.wait_stopped().await;

        // Named (registered in default system)
        let h = my_actor.clone().spawn().named("worker-1").await?;
        assert!(default_system.get::<MyActor>("worker-1").is_some());
        h.stop(StopReason::Graceful).await?;
        h.wait_stopped().await;
        name_free(&default_system, "worker-1").await; // setup

        // Named with custom mailbox
        let config = ActorConfig::default().with_mailbox_capacity(256);
        let h = my_actor
            .clone()
            .spawn()
            .named("worker-1")
            .with_config(config)
            .await?;
        assert_eq!(h.mailbox_capacity(), 256);
        h.stop(StopReason::Graceful).await?;
        h.wait_stopped().await;
        name_free(&default_system, "worker-1").await; // setup

        // On a specific system
        let sys = ActorSystem::create("my-system")?;
        let h = my_actor
            .clone()
            .spawn()
            .named("worker-1")
            .on_system(&sys)
            .await?;
        assert!(sys.get::<MyActor>("worker-1").is_some());
        assert!(default_system.get::<MyActor>("worker-1").is_none());
        h.stop(StopReason::Graceful).await?;
        h.wait_stopped().await;

        // Supervisor (children restart via OneForOne, 3 restarts / 5s)
        let h = my_actor
            .clone()
            .spawn()
            .named("supervisor")
            .supervisor()
            .await?;
        h.stop(StopReason::Graceful).await?;
        h.wait_stopped().await;
        name_free(&default_system, "supervisor").await; // setup

        // Supervisor with custom strategy
        let sup = SupervisionConfig::one_for_all().max_restarts(10, Duration::from_secs(60));
        let h = my_actor
            .clone()
            .spawn()
            .named("supervisor")
            .with_supervision(sup)
            .await?;
        h.stop(StopReason::Graceful).await?;
        h.wait_stopped().await;
        Ok(())
    });
}

/// README: "### Actor Registry (ActorSystem)". The block rebinds `sys` to the
/// custom system `game`, so the name operations need actors there (setup).
#[test]
fn quick_start_actor_registry() {
    run_block(async {
        use std::time::Duration;
        use tokio_actors::{ActorSystem, ShutdownPolicy, SystemConfig};

        // Default system (lazy singleton)
        let sys = ActorSystem::default();
        assert_eq!(sys.name(), "default");

        // Custom system with config
        let sys = ActorSystem::create_with(
            "game",
            SystemConfig {
                shutdown_policy: ShutdownPolicy {
                    timeout: Duration::from_secs(60),
                    per_actor_timeout: Duration::from_secs(10),
                },
                ..Default::default()
            },
        )?;
        // setup: the actor the comment below assumes.
        let worker = MyActor.spawn().named("worker-1").on_system(&sys).await?;

        // The calls below assume actors spawned in "game": "worker-1" for the name calls (stop and kill
        // are alternatives, as each removes it) and any actor for the id calls.

        // Named lookup (OTP whereis/1)
        let handle = sys.get::<MyActor>("worker-1");
        assert!(handle.as_ref() == Some(&worker));

        // Stop/kill by name
        sys.stop("worker-1").await?; // Graceful (vetoable)
                                     // setup: the stop and kill lines are alternatives: the stopped actor leaves, and the next one takes its name.
        worker.wait_stopped().await;
        name_free(&sys, "worker-1").await;
        let worker = MyActor.spawn().named("worker-1").on_system(&sys).await?;
        sys.kill("worker-1").await?; // Force: no hook begins once the Kill is read
        assert_eq!(worker.wait_stopped().await, StopReason::Kill);
        // setup: an actor for the id operations.
        let anonymous = MyActor.spawn().on_system(&sys).await?;

        // Every actor, named or anonymous, is enumerable and killable by id
        let ids = sys.actor_ids();
        let status = sys.actor_status(&ids[0]);
        let report = sys.kill_by_id(&ids[0]).await; // Option<StopReport>: None when the id is not registered or its actor refuses the Kill; otherwise the target takes the same Kill, kill grace and abort as with kill(name), and this call also waits for its end (a short settle wait after an abort) to report it
        assert_eq!(ids, vec![anonymous.id().clone()]);
        assert!(status.is_some());
        let report = report.expect("a registered id answers a report");
        assert_eq!(report.reason, Some(StopReason::Kill));
        // setup: a root for the shutdown to stop.
        let root = MyActor.spawn().named("root").on_system(&sys).await?;

        // Coordinated shutdown: stops root actors only (each supervisor tears down
        // its own subtree), takes every root through its stop ladder, and reports
        let report = sys.shutdown().await;
        assert!(report
            .outcomes
            .iter()
            .any(|(id, stop)| id == root.id() && stop.outcome == StopOutcome::Graceful));
        assert_eq!(root.wait_stopped().await, StopReason::ParentRequest);
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Core Concepts: Supervision
// ---------------------------------------------------------------------------

/// Supervised workers of the supervision blocks.
#[derive(Default)]
struct Worker;

enum WorkerMsg {
    Ping,
    Crash,
}

impl Actor for Worker {
    type Message = WorkerMsg;
    type Response = ();

    async fn handle(&mut self, msg: WorkerMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            WorkerMsg::Ping => Ok(()),
            WorkerMsg::Crash => panic!("worker crash"),
        }
    }
}

/// README: "### Supervision", first block.
#[test]
#[allow(clippy::redundant_closure, clippy::default_constructed_unit_structs)]
fn core_concepts_supervision() {
    run_block(async {
        use std::time::Duration;
        use tokio_actors::{
            actor::{context::ActorContext, Actor, ActorExt},
            ActorResult, ChildEvent, RestartType, Shutdown, SupervisionConfig,
        };

        // setup: the second child type, and the `Worker` above.
        struct CacheActor;
        impl CacheActor {
            fn new() -> Self {
                CacheActor
            }
        }
        impl Actor for CacheActor {
            type Message = ();
            type Response = ();
            async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
                Ok(())
            }
        }

        struct MySupervisor;

        impl Actor for MySupervisor {
            type Message = ();
            type Response = ();

            async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
                // Permanent - always restarts, even after graceful stop
                ctx.spawn_child(|| Worker::default())
                    .named("worker")
                    .await?;

                // Transient - restarts on crash, stays down on graceful stop
                ctx.spawn_child(|| CacheActor::new())
                    .named("cache")
                    .restart_type(RestartType::Transient)
                    .shutdown(Shutdown::Timeout(Duration::from_secs(10)))
                    .await?;

                Ok(())
            }

            async fn on_child_stopped(
                &mut self,
                event: &ChildEvent,
                _ctx: &mut ActorContext<Self>,
            ) -> ActorResult<()> {
                println!("Child {} stopped: {:?}", event.child_id, event.action);
                Ok(())
            }

            // ...
            async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
                Ok(())
            }
        }

        // Launch with OneForAll strategy and custom budget
        let sup = SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(30));
        // setup: a system of its own, so the child names are free.
        let sys = ActorSystem::create("readme-supervision")?;
        let handle = MySupervisor
            .spawn()
            .named("my-sup")
            .with_supervision(sup)
            .on_system(&sys)
            .await?;

        // The runtime handles the restart loop: both children run under the
        // supervisor, and a crashed permanent child comes back.
        assert_eq!(handle.get_status().await?.child_count, 2);
        let worker = sys
            .get::<Worker>("worker")
            .expect("the worker is registered");
        assert!(matches!(
            worker.send(WorkerMsg::Crash).await,
            Err(AskError::Actor(ActorError::Panic(_)))
        ));
        assert!(matches!(
            worker.wait_stopped().await,
            StopReason::Failure(_)
        ));
        wait_until("the worker to be running again", || {
            sys.get::<Worker>("worker")
                .is_some_and(|w| w.status() == ActorStatus::Running)
        })
        .await;
        let again = sys.get::<Worker>("worker").unwrap();
        assert!(again == worker, "the restarted worker keeps its ActorId");
        again.send(WorkerMsg::Ping).await?;
        assert_eq!(handle.get_status().await?.child_count, 2);
        handle.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

/// README: "#### Managing Children", the handler block.
#[test]
fn core_concepts_managing_children() {
    run_block(async {
        enum Cmd {
            Pause,
            Resume,
        }

        // setup: a supervisor with a `worker` child.
        struct Boss;
        impl Actor for Boss {
            type Message = Cmd;
            type Response = ();

            async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
                ctx.spawn_child(Worker::default).named("worker").await?;
                Ok(())
            }

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
        }

        let sys = ActorSystem::create("readme-managing-children")?;
        let boss = Boss.spawn().supervisor().on_system(&sys).await?;
        let first = sys.get::<Worker>("worker").expect("the worker runs");

        boss.send(Cmd::Pause).await?;
        assert_eq!(first.wait_stopped().await, StopReason::ParentRequest);
        // The child stays down: no restart, and the spec is kept.
        assert_eq!(boss.get_status().await?.child_count, 1);
        assert!(!first.is_alive());

        boss.send(Cmd::Resume).await?;
        wait_until("the resumed worker", || {
            sys.get::<Worker>("worker")
                .is_some_and(|w| w.status() == ActorStatus::Running)
        })
        .await;
        let resumed = sys.get::<Worker>("worker").unwrap();
        assert!(resumed == first, "same ActorId after restart_child");
        resumed.send(WorkerMsg::Ping).await?;
        boss.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Core Concepts: Crash Semantics
// ---------------------------------------------------------------------------

/// The actor of the crash-semantics block: `worker.send(job)`.
struct Crashy;

#[derive(Debug, Clone, Copy)]
enum Job {
    Fine,
    Panic,
    Fail,
}

impl Actor for Crashy {
    type Message = Job;
    type Response = u32;

    async fn handle(&mut self, job: Job, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        match job {
            Job::Fine => Ok(7),
            Job::Panic => panic!("job panicked"),
            Job::Fail => Err(ActorError::user("job failed")),
        }
    }
}

/// README: "### Crash Semantics": the match, as written, on one function so
/// that each scenario runs it.
async fn crash_semantics_block(worker: &ActorHandle<Crashy>, job: Job) {
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
}

/// README: "### Crash Semantics", the match block and the paragraph below it.
#[test]
fn core_concepts_crash_semantics() {
    run_block(async {
        let sys = ActorSystem::create("readme-crash")?;

        let worker = Crashy.spawn().on_system(&sys).await?;
        crash_semantics_block(&worker, Job::Fine).await;
        assert_eq!(take_output(), vec!["done: 7".to_string()]);

        // A handler `Err` is a crash: the actor stops, and a send caller sees it.
        crash_semantics_block(&worker, Job::Fail).await;
        assert_eq!(
            take_output(),
            vec!["handler returned Err: actor logic error: job failed".to_string()]
        );
        assert!(matches!(
            worker.wait_stopped().await,
            StopReason::Failure(ActorError::User(_))
        ));

        // A panic is caught at the callback boundary and shown to the caller.
        let worker = Crashy.spawn().named("crashy-panic").on_system(&sys).await?;
        crash_semantics_block(&worker, Job::Panic).await;
        assert!(
            sys.get::<Crashy>("crashy-panic").is_none(),
            "the name is free once the panic is read"
        );
        let lines = take_output();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("worker crashed: ") && lines[0].contains("job panicked"));
        assert!(matches!(
            worker.wait_stopped().await,
            StopReason::Failure(ActorError::Panic(_))
        ));

        // A send to the stopped actor finds the mailbox closed.
        crash_semantics_block(&worker, Job::Fine).await;
        assert_eq!(
            take_output(),
            vec!["mailbox closed (actor down or mid-restart)".to_string()]
        );

        // On the notify path `Err` and a panic stop the actor the same way,
        // while the caller learns nothing.
        for job in [Job::Fail, Job::Panic] {
            let worker = Crashy.spawn().on_system(&sys).await?;
            worker.notify(job).await?;
            assert!(matches!(
                worker.wait_stopped().await,
                StopReason::Failure(_)
            ));
        }
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Core Concepts: termination
// ---------------------------------------------------------------------------

/// Records which hooks of a stop ran, and vetoes a graceful stop.
struct Tiers {
    events: mpsc::UnboundedSender<String>,
    on_stopped_gate: Option<Arc<Notify>>,
}

impl Actor for Tiers {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        let _ = self.events.send(format!("pre_stop {reason}"));
        !matches!(reason, StopReason::Graceful)
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.events.send(format!("on_stopped {reason}"));
        if let Some(gate) = &self.on_stopped_gate {
            gate.notified().await;
        }
        Ok(())
    }
}

/// README: "### 3-Tier Termination": the three lines, as written, on one
/// handle. The actor vetoes the graceful stop and holds in `on_stopped`, so
/// each line meets the state it names.
#[test]
fn core_concepts_three_tier_termination() {
    run_block(async {
        use tokio_actors::StopReason;

        let (events, _seen) = mpsc::unbounded_channel();
        let gate = Arc::new(Notify::new());
        let sys = ActorSystem::create("readme-tiers")?;
        // setup: a handle.
        let handle = Tiers {
            events,
            on_stopped_gate: Some(gate),
        }
        .spawn()
        .on_system(&sys)
        .await?;

        handle.stop(StopReason::Graceful).await?; // Tier 1: pre_stop can veto
        handle.stop(StopReason::Cancelled).await?; // Tier 2: non-vetoable, on_stopped runs
        handle.stop(StopReason::Kill).await?; // Tier 3: not vetoable; no hook begins once it is read

        // A stop request only strengthens, so whichever of the three the actor
        // read first, the Kill is the final reason.
        assert_eq!(handle.wait_stopped().await, StopReason::Kill);
        Ok(())
    });
}

/// README: "### 3-Tier Termination", each tier on its own actor.
#[test]
fn core_concepts_three_tier_termination_tiers() {
    run_block(async {
        let sys = ActorSystem::create("readme-tiers-each")?;
        let spawn = |gate: Option<Arc<Notify>>| {
            let (events, seen) = mpsc::unbounded_channel();
            let sys = sys.clone();
            async move {
                let handle = Tiers {
                    events,
                    on_stopped_gate: gate,
                }
                .spawn()
                .on_system(&sys)
                .await
                .unwrap();
                (handle, seen)
            }
        };

        // Tier 1: pre_stop vetoes, the actor keeps running.
        let (handle, mut seen) = spawn(None).await;
        handle.stop(StopReason::Graceful).await?;
        assert_eq!(seen.recv().await.as_deref(), Some("pre_stop graceful"));
        assert_eq!(handle.get_status().await?.status, ActorStatus::Running);
        assert!(handle.is_alive());

        // Tier 2: not vetoable, and on_stopped runs with the reason.
        handle.stop(StopReason::Cancelled).await?;
        assert_eq!(handle.wait_stopped().await, StopReason::Cancelled);
        let mut events = Vec::new();
        while let Ok(event) = seen.try_recv() {
            events.push(event);
        }
        assert!(events.contains(&"on_stopped cancelled".to_string()));

        // Tier 3: no hook begins once the Kill is read.
        let (handle, mut seen) = spawn(None).await;
        handle.stop(StopReason::Kill).await?;
        assert_eq!(handle.wait_stopped().await, StopReason::Kill);
        assert!(seen.try_recv().is_err(), "a hook began after the Kill");
        Ok(())
    });
}

/// An actor for the bounded-stop block: it vetoes a graceful stop when told.
struct Stubborn {
    veto: bool,
}

impl Actor for Stubborn {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        !(self.veto && matches!(reason, StopReason::Graceful))
    }
}

/// README: "### Bounded Stop": the match, as written, against a cooperative
/// actor and against one whose `pre_stop` vetoes every graceful stop.
#[test]
fn core_concepts_bounded_stop() {
    run_paused_block(async {
        let sys = ActorSystem::create("readme-bounded-stop")?;

        // setup: a cooperative actor.
        let handle = Stubborn { veto: false }.spawn().on_system(&sys).await?;
        {
            use std::time::Duration;
            use tokio_actors::{StopOutcome, StopReason};

            match handle
                .stop_within(StopReason::Graceful, Duration::from_secs(5))
                .await?
            {
                StopOutcome::Graceful => println!("stopped without this call's Kill taking effect"),
                StopOutcome::Killed => println!("stopped after this call's Kill was taken"),
                StopOutcome::Aborted => println!("its task ended without publishing Stopped"),
                StopOutcome::Unresponsive => {
                    println!("no terminal status within the call's last bound")
                }
            }
        }
        assert_eq!(
            take_output(),
            vec!["stopped without this call's Kill taking effect".to_string()]
        );

        // setup: an actor that vetoes, so the window ends in this call's Kill.
        let handle = Stubborn { veto: true }.spawn().on_system(&sys).await?;
        {
            use std::time::Duration;
            use tokio_actors::{StopOutcome, StopReason};

            match handle
                .stop_within(StopReason::Graceful, Duration::from_millis(50))
                .await?
            {
                StopOutcome::Graceful => println!("stopped without this call's Kill taking effect"),
                StopOutcome::Killed => println!("stopped after this call's Kill was taken"),
                StopOutcome::Aborted => println!("its task ended without publishing Stopped"),
                StopOutcome::Unresponsive => {
                    println!("no terminal status within the call's last bound")
                }
            }
        }
        assert_eq!(
            take_output(),
            vec!["stopped after this call's Kill was taken".to_string()]
        );
        assert_eq!(handle.wait_stopped().await, StopReason::Kill);
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Core Concepts: Lifecycle Hooks
// ---------------------------------------------------------------------------

/// README: "### Lifecycle Hooks".
#[test]
fn core_concepts_lifecycle_hooks() {
    run_block(async {
        // setup: the message the block schedules, and the database it closes.
        #[derive(Clone)]
        struct HealthCheck;
        struct Database {
            closed: Arc<AtomicBool>,
        }
        impl Database {
            async fn close(&self) {
                self.closed.store(true, Ordering::SeqCst);
            }
        }
        struct Service {
            database: Database,
        }

        impl Actor for Service {
            type Message = HealthCheck;
            type Response = ();

            async fn handle(
                &mut self,
                _: HealthCheck,
                _ctx: &mut ActorContext<Self>,
            ) -> ActorResult<()> {
                Ok(())
            }

            async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
                // Initialize state, schedule timers
                ctx.schedule(HealthCheck)
                    .every(Duration::from_secs(30))
                    .await?;
                Ok(())
            }

            async fn on_stopped(
                &mut self,
                _reason: &StopReason,
                ctx: &mut ActorContext<Self>,
            ) -> ActorResult<()> {
                // Cleanup resources
                self.database.close().await;
                Ok(())
            }
        }

        let closed = Arc::new(AtomicBool::new(false));
        let sys = ActorSystem::create("readme-hooks")?;
        let handle = Service {
            database: Database {
                closed: closed.clone(),
            },
        }
        .spawn()
        .on_system(&sys)
        .await?;
        assert_eq!(handle.get_status().await?.timer_count, 1);
        assert!(!closed.load(Ordering::SeqCst));
        handle.stop(StopReason::Graceful).await?;
        handle.wait_stopped().await;
        assert!(closed.load(Ordering::SeqCst));
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Core Concepts: Message Passing
// ---------------------------------------------------------------------------

/// README: "### Message Passing: `notify` vs `send`".
#[test]
fn core_concepts_message_passing_notify_vs_send() {
    run_block(async {
        let sys = ActorSystem::create("readme-message-passing")?;
        // setup: a handle, and a message that can be sent four times.
        let handle = Summer::default().spawn().on_system(&sys).await?;
        let msg = 1u32;

        // Fire-and-forget (async until mailbox accepts)
        handle.notify(msg).await?;

        // Request-response (wait for actor to process)
        let response = handle.send(msg).await?;
        assert_eq!(response, 2);

        // Request-response bounded by a single deadline
        let response = handle.send_timeout(msg, Duration::from_secs(2)).await?;
        assert_eq!(response, 3);

        // Non-blocking attempt (returns immediately)
        handle.try_notify(msg)?;

        // The mailbox is in order, so this send sees all four messages.
        assert_eq!(handle.send(0).await?, 4);
        handle.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

/// README: "### Message Passing" error-handling nuance: `Err` from `handle`
/// stops the actor on every path.
#[test]
fn core_concepts_message_passing_error_stops_on_every_path() {
    run_block(async {
        let sys = ActorSystem::create("readme-message-passing-err")?;
        for path in 0..3 {
            let handle = Crashy.spawn().on_system(&sys).await?;
            match path {
                0 => handle.notify(Job::Fail).await?,
                1 => {
                    assert!(matches!(
                        handle.send(Job::Fail).await,
                        Err(AskError::Actor(ActorError::User(_)))
                    ));
                }
                _ => {
                    assert!(matches!(
                        handle.send_timeout(Job::Fail, Duration::from_secs(5)).await,
                        Err(AskError::Actor(ActorError::User(_)))
                    ));
                }
            }
            assert!(matches!(
                handle.wait_stopped().await,
                StopReason::Failure(ActorError::User(_))
            ));
        }
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Core Concepts: Priority Lane
// ---------------------------------------------------------------------------

/// README: "### Priority Lane". Existing pins for the paragraphs below the
/// block: `tests/priority_lane.rs`
/// (`same_sender_priority_message_overtakes_its_own_earlier_normal_message`,
/// `priority_calls_degrade_to_the_mailbox_without_a_lane`,
/// `status_reports_the_priority_lane_separately`,
/// `a_priority_flood_delays_the_normal_mailbox_and_loses_no_message`).
#[test]
fn core_concepts_priority_lane() {
    let _lock = default_system_lock();
    run_block(async {
        use std::time::Duration;
        use tokio_actors::{actor::ActorExt, ActorConfig};

        // setup: `my_actor` and `msg`.
        let my_actor = MyActor;
        let msg = 5u32;

        let config = ActorConfig::default().with_priority_lane(16);
        let h = my_actor
            .spawn()
            .named("worker-1")
            .with_config(config)
            .await?;

        h.notify_priority(msg).await?; // fire-and-forget
        h.try_notify_priority(msg)?; // non-blocking attempt
        let response = h.send_priority(msg).await?; // request-response
        let response = h.send_priority_timeout(msg, Duration::from_secs(2)).await?;

        h.priority_len(); // 0 when no lane is enabled
        h.priority_capacity(); // 0 when no lane is enabled

        assert_eq!(response, 5);
        assert_eq!(h.priority_capacity(), 16);
        h.stop(StopReason::Graceful).await?;
        h.wait_stopped().await;
        name_free(&ActorSystem::default(), "worker-1").await;
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Core Concepts: Timers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum TimerMsg {
    Start,
    Tick,
}

struct Timed {
    ticks: mpsc::UnboundedSender<()>,
}

impl Actor for Timed {
    type Message = TimerMsg;
    type Response = ();

    async fn handle(&mut self, trigger: TimerMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match trigger {
            TimerMsg::Tick => {
                let _ = self.ticks.send(());
                Ok(())
            }
            TimerMsg::Start => {
                use tokio::time::Duration;
                use tokio_actors::MissPolicy;

                // setup: the message the block schedules.
                let msg = TimerMsg::Tick;

                // `msg` is a Copy message value; pass `msg.clone()` for a Clone message that is not Copy, and use `every_with` for one that is not Clone.

                // One-shot after delay
                ctx.schedule(msg).after(Duration::from_secs(5)).await?;

                // Recurring - default MissPolicy::Skip
                ctx.schedule(msg).every(Duration::from_millis(100)).await?;

                // Recurring with explicit drift strategy
                ctx.schedule(msg)
                    .every(Duration::from_millis(100))
                    .on_miss(MissPolicy::CatchUp)
                    .await?;
                Ok(())
            }
        }
    }
}

/// README: "### Timers with Drift Control". Scheduling in the past is covered
/// by `scheduling_in_the_past_fires_immediately`.
#[test]
fn core_concepts_timers_with_drift_control() {
    run_block(async {
        let (ticks, mut seen) = mpsc::unbounded_channel();
        let sys = ActorSystem::create("readme-timers")?;
        let handle = Timed { ticks }.spawn().on_system(&sys).await?;
        handle.send(TimerMsg::Start).await?;

        // Three timers: the one-shot and the two recurring ones.
        assert_eq!(handle.get_status().await?.timer_count, 3);
        // Both recurring timers tick; the one-shot (5 s) is still pending.
        for _ in 0..4 {
            seen.recv().await.expect("a recurring tick");
        }
        assert_eq!(handle.get_status().await?.timer_count, 3);
        handle.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

/// README: "### Timers with Drift Control", the Edge Case line: a time in the
/// past fires immediately, with no panic.
#[test]
fn scheduling_in_the_past_fires_immediately() {
    struct Past {
        fired: mpsc::UnboundedSender<u8>,
    }
    impl Actor for Past {
        type Message = u8;
        type Response = ();
        async fn handle(&mut self, msg: u8, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if msg == 0 {
                let past = tokio::time::Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .expect("the clock has run for a second");
                ctx.schedule(1).at(past).await?;
                ctx.schedule(2).after(Duration::ZERO).await?;
            } else {
                let _ = self.fired.send(msg);
            }
            Ok(())
        }
    }

    run_block(async {
        let (fired, mut seen) = mpsc::unbounded_channel();
        let sys = ActorSystem::create("readme-timers-past")?;
        let handle = Past { fired }.spawn().on_system(&sys).await?;
        handle.send(0).await?;
        let mut got = [seen.recv().await.unwrap(), seen.recv().await.unwrap()];
        got.sort_unstable();
        assert_eq!(got, [1, 2]);
        handle.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Core Concepts: Mailbox Monitoring
// ---------------------------------------------------------------------------

/// README: "### Mailbox Monitoring", as written, on a function of the handle.
/// The block's `use tracing::{error, warn};` is left out: the `tracing`
/// dependency is optional here, and the file's own `warn!` and `error!`
/// record the lines so the test can assert them.
async fn mailbox_monitoring_block(handle: ActorHandle<MyActor>) -> TestResult {
    if handle.mailbox_available() < 10 {
        warn!("Actor {} is backed up!", handle.id());
    }

    if !handle.is_alive() {
        error!("Actor {} has stopped!", handle.id());
    }

    // System channel bypasses the mailbox, works even when full
    let status = handle.get_status().await?;
    Ok(())
}

/// README: "### Mailbox Monitoring".
#[test]
fn core_concepts_mailbox_monitoring() {
    run_block(async {
        let sys = ActorSystem::create("readme-mailbox-monitoring")?;

        // A roomy mailbox, alive: neither line fires.
        let roomy = MyActor.spawn().on_system(&sys).await?;
        mailbox_monitoring_block(roomy.clone()).await?;
        assert!(take_output().is_empty());

        // A mailbox with fewer than ten free slots reads as backed up.
        let small = MyActor
            .spawn()
            .with_config(ActorConfig::default().with_mailbox_capacity(8))
            .on_system(&sys)
            .await?;
        mailbox_monitoring_block(small.clone()).await?;
        assert_eq!(
            take_output(),
            vec![format!("Actor {} is backed up!", small.id())]
        );

        // A stopped actor reads as stopped.
        roomy.stop(StopReason::Graceful).await?;
        roomy.wait_stopped().await;
        assert!(mailbox_monitoring_block(roomy.clone()).await.is_err());
        assert_eq!(
            take_output(),
            vec![format!("Actor {} has stopped!", roomy.id())]
        );
        small.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Core Concepts: Turn Watchdog
// ---------------------------------------------------------------------------

/// An actor whose turn is held open until released.
struct Held {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    finished: Arc<AtomicBool>,
}

impl Actor for Held {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// README: "### Turn Watchdog". Stuck-turn semantics are tested in
/// `tests/turn_watchdog.rs` (`turn_elapsed_reports_a_running_turn`,
/// `stuck_actors_lists_turns_over_the_threshold`,
/// `stuck_actors_is_empty_without_a_threshold`). The system is named
/// `watchdog-game` because `game` belongs to the registry block.
#[test]
fn core_concepts_turn_watchdog() {
    run_block(async {
        use std::time::Duration;
        use tokio_actors::{ActorSystem, SystemConfig};

        let config = SystemConfig::default().with_turn_warning_threshold(Duration::from_secs(1));
        let sys = ActorSystem::create_with("watchdog-game", config)?;
        assert_eq!(sys.turn_warning_threshold(), Some(Duration::from_secs(1)));
        // setup: an actor for the id below.
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let held = Held {
            entered: entered.clone(),
            release: release.clone(),
            finished: Arc::default(),
        }
        .spawn()
        .on_system(&sys)
        .await?;

        // Or, on any system, at any time:
        sys.set_turn_warning_threshold(Some(Duration::from_secs(1)));

        let ids = sys.actor_ids();

        // Between callbacks nothing is elapsed.
        assert_eq!(sys.turn_elapsed(&ids[0]), None);
        assert!(sys.stuck_actors().is_empty());

        // Inside a callback that began after the threshold was set, it is.
        held.notify(()).await?;
        entered.notified().await;
        sys.turn_elapsed(&ids[0]); // Some(d) while that actor is inside a callback that began after a threshold was set
        assert!(sys.turn_elapsed(&ids[0]).is_some());
        sys.stuck_actors(); // every actor over the threshold, longest first

        // It is observation only: the actor still stops once released.
        release.notify_one();
        held.stop(StopReason::Graceful).await?;
        held.wait_stopped().await;
        assert_eq!(sys.turn_elapsed(&ids[0]), None);
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Core Concepts: Telemetry
// ---------------------------------------------------------------------------

/// README: "### Telemetry". The handler and the subscription are each shown
/// the end of an actor spawned after them, and the snapshot lists the
/// subscriber. Handlers, subscriptions, the snapshot and the monitor are
/// tested in `tests/telemetry_handlers.rs`, `tests/telemetry_subscription.rs`,
/// `tests/telemetry_snapshot.rs` and `tests/system_monitor.rs`.
#[test]
fn core_concepts_telemetry() {
    run_block(async {
        use tokio_actors::telemetry::{Filter, KindSet, Scope, TelemetryEvent};

        // setup: the system, and the channel the subscriber below reports on.
        let sys = ActorSystem::create("telemetry-example")?;
        let (seen_tx, mut seen) = mpsc::unbounded_channel();

        // A handler is a plain closure. It must not block: hand slow work to a channel.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<TelemetryEvent<'static>>(1024);
        sys.attach_telemetry("export", KindSet::cold(), move |event| {
            let _ = tx.try_send(event.to_static());
        });
        // A task of yours reads `rx` and does the slow work there.

        // setup: the actor that subscribes, and its message.
        enum Msg {
            Event(tokio_actors::telemetry::Delivery),
        }
        struct Watcher(mpsc::UnboundedSender<tokio_actors::telemetry::Delivery>);

        impl Actor for Watcher {
            type Message = Msg;
            type Response = ();

            async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
                // A subscription, made inside a callback: the selected events arrive as this actor's own messages.
                let id = ctx.subscribe(Filter::new(Scope::System), Msg::Event);
                Ok(())
            }

            async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
                let Msg::Event(delivery) = msg;
                let _ = self.0.send(delivery);
                Ok(())
            }
        }
        let watcher = Watcher(seen_tx).spawn().on_system(&sys).await?;

        // setup: an actor that starts and ends after the handler and the subscription exist.
        let subject = MyActor.spawn().on_system(&sys).await?;
        let subject_id = subject.id().clone();
        subject.stop(StopReason::Graceful).await?;
        subject.wait_stopped().await;
        let is_its_end = |event: &TelemetryEvent<'_>| matches!(event, TelemetryEvent::ActorEnded { actor, .. } if actor.id() == &subject_id);

        // The handler's channel carries the end of that actor.
        let by_handler = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match rx.recv().await {
                    Some(event) if is_its_end(&event) => break true,
                    Some(_) => {}
                    None => break false,
                }
            }
        })
        .await;
        assert_eq!(by_handler, Ok(true), "the handler was not shown the end");

        // And the subscriber handled it as one of its own messages.
        let by_subscription = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match seen.recv().await {
                    Some(delivery) if is_its_end(&delivery.event) => break true,
                    Some(_) => {}
                    None => break false,
                }
            }
        })
        .await;
        assert_eq!(
            by_subscription,
            Ok(true),
            "the subscriber was not sent the end"
        );

        // A record of every registered actor that has not ended, read without asking any of them.
        for record in sys.snapshot() {
            println!(
                "{}: {:?}, mailbox {} of {}",
                record.actor.id(),
                record.status,
                record.mailbox_len,
                record.mailbox_capacity
            );
        }
        let listed = format!("{}: Running, mailbox ", watcher.id());
        let out = take_output();
        assert!(out.iter().any(|line| line.starts_with(&listed)), "{out:?}");

        assert!(sys.detach_telemetry("export"));
        watcher.stop(StopReason::Graceful).await?;
        watcher.wait_stopped().await;
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Core Concepts: Blocking Work
// ---------------------------------------------------------------------------

/// README: "### Blocking Work".
#[test]
fn core_concepts_blocking_work() {
    run_block(async {
        // setup: the job, its function, and the actor around the block.
        enum Job {
            Run(Arc<AtomicUsize>),
            Panic,
        }
        fn run_job(msg: Job) {
            match msg {
                Job::Run(done) => {
                    done.fetch_add(1, Ordering::SeqCst);
                }
                Job::Panic => panic!("job panicked on the blocking pool"),
            }
        }
        struct Offloader;
        impl Actor for Offloader {
            type Message = Job;
            type Response = ();

            // Inside any actor:
            async fn handle(&mut self, msg: Job, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
                tokio::task::spawn_blocking(move || run_job(msg))
                    .await
                    .map_err(|e| ActorError::user(e.to_string()))?;
                Ok(())
            }
        }

        let sys = ActorSystem::create("readme-blocking")?;
        let handle = Offloader.spawn().on_system(&sys).await?;
        let done = Arc::new(AtomicUsize::new(0));
        handle.send(Job::Run(done.clone())).await?;
        assert_eq!(done.load(Ordering::SeqCst), 1);

        // A job that panics on the pool becomes the handler's `Err`.
        assert!(matches!(
            handle.send(Job::Panic).await,
            Err(AskError::Actor(ActorError::User(_)))
        ));
        assert!(matches!(
            handle.wait_stopped().await,
            StopReason::Failure(ActorError::User(_))
        ));
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Deep Rust Patterns: ActorHandle Equality
// ---------------------------------------------------------------------------

/// README: "### ActorHandle Equality".
// Clippy reads a handle as a mutable key type, though its `Hash` and `Eq` use
// the id and the system name only, which never change.
#[allow(clippy::mutable_key_type)]
#[test]
fn deep_rust_patterns_actor_handle_equality() {
    let _lock = default_system_lock();
    run_block(async {
        let actor1 = MyActor.spawn().named("foo").await?;
        let actor2 = actor1.clone();

        assert_eq!(actor1, actor2); // Same actor ID

        let actor3 = MyActor.spawn().named("bar").await?;
        assert_ne!(actor1, actor3); // Different actor ID

        // Handles are usable in a HashSet for deduplication.
        let set: std::collections::HashSet<_> = [actor1.clone(), actor2.clone(), actor3.clone()]
            .into_iter()
            .collect();
        assert_eq!(set.len(), 2);

        // The comparison names the owning system as well as the id.
        let other = ActorSystem::create("readme-equality-other")?;
        let foreign = MyActor.spawn().named("foo").on_system(&other).await?;
        assert_eq!(foreign.id(), actor1.id());
        assert_ne!(foreign, actor1);

        for h in [&actor1, &actor3, &foreign] {
            h.stop(StopReason::Graceful).await?;
        }
        Ok(())
    });
}

/// README: "### Bounded Mailbox Backpressure" (and "Bounded Mailboxes =
/// Natural Backpressure"): the default capacity is 64, `notify` and `send`
/// wait when the mailbox is full, and `try_notify` returns `Full` at once.
#[test]
fn deep_rust_patterns_bounded_mailbox_backpressure() {
    run_block(async {
        let sys = ActorSystem::create("readme-backpressure")?;
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let h = Gated {
            entered: entered.clone(),
            release: release.clone(),
        }
        .spawn()
        .on_system(&sys)
        .await?;
        assert_eq!(h.mailbox_capacity(), 64);

        // One message holds the turn, then the mailbox fills.
        h.notify(0).await?;
        entered.notified().await;
        for n in 0..64 {
            h.try_notify(n)?;
        }
        assert_eq!(h.mailbox_available(), 0);
        assert!(matches!(h.try_notify(64), Err(TrySendError::Full)));
        let notify = tokio::time::timeout(Duration::ZERO, h.notify(64)).await;
        assert!(notify.is_err(), "notify waits for room");
        let send = tokio::time::timeout(Duration::ZERO, h.send(64)).await;
        assert!(send.is_err(), "send waits for room");

        release.notify_waiters();
        h.stop(StopReason::Kill).await?;
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// API at a Glance
// ---------------------------------------------------------------------------

/// README: "### SpawnBuilder Chain".
#[test]
#[rustfmt::skip]
fn api_at_a_glance_spawn_builder_chain() {
    run_block(async {
        // setup: the names the chain uses.
        let actor = MyActor;
        let sys = ActorSystem::create("readme-spawn-chain")?;
        let config = ActorConfig::default().with_mailbox_capacity(32);
        let sup_config = SupervisionConfig::one_for_all();
        let dur = Duration::from_secs(5);

        let handle = actor.spawn()                     // Start the builder
            .named("my-actor")            // Optional: assign a name/ID
            .on_system(&sys)              // Optional: target a specific ActorSystem
            .with_config(config)          // Optional: custom ActorConfig
            .supervisor()                 // Optional: supervise children (default config)
            .with_supervision(sup_config) // Optional: enable supervision (custom config)
            .start_timeout(dur)           // Optional: bound init instead of waiting forever
            .kill_grace(dur)              // Optional: time between a Kill and the abort (default 120 ms)
            .detached()                   // Optional, instead of .start_timeout: skip the init ack
            .await?;                      // Finalize: registers and spawns the task, then awaits pre_start + on_started (not when detached)

        assert_eq!(handle.mailbox_capacity(), 32);
        assert!(sys.get::<MyActor>("my-actor").is_some());
        handle.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

/// An actor whose `pre_start` waits for a release, or fails.
struct SlowStart {
    entered: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
    fail: bool,
}

impl Actor for SlowStart {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        if let Some(release) = self.release.take() {
            let _ = release.await;
        }
        if self.fail {
            return Err(ActorError::user("pre_start failed"));
        }
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// README: "### SpawnBuilder Chain", the paragraphs below the block: the init
/// ack option called last wins, and a detached start that fails shows only as
/// a handle whose next call returns `Closed`.
#[test]
fn api_at_a_glance_spawn_builder_init_ack() {
    run_block(async {
        let sys = ActorSystem::create("readme-spawn-ack")?;
        let dur = Duration::from_secs(30);

        // `.start_timeout(dur)` called last: the spawn waits for `pre_start`.
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let spawn = std::future::IntoFuture::into_future(
            SlowStart {
                entered: Some(entered_tx),
                release: Some(release_rx),
                fail: false,
            }
            .spawn()
            .on_system(&sys)
            .detached()
            .start_timeout(dur),
        );
        tokio::pin!(spawn);
        tokio::select! {
            biased;
            _ = &mut spawn => panic!("the spawn resolved before pre_start returned"),
            _ = entered_rx => {}
        }
        release_tx.send(()).unwrap();
        let handle = spawn.await?;
        handle.stop(StopReason::Graceful).await?;

        // `.detached()` called last: the spawn resolves while `pre_start` waits.
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let handle = SlowStart {
            entered: Some(entered_tx),
            release: Some(release_rx),
            fail: false,
        }
        .spawn()
        .on_system(&sys)
        .start_timeout(dur)
        .detached()
        .await?;
        entered_rx.await?;
        release_tx.send(()).unwrap();
        handle.send(()).await?;
        handle.stop(StopReason::Graceful).await?;

        // A detached start whose `pre_start` fails: the handle's next call
        // returns Closed.
        let handle = SlowStart {
            entered: None,
            release: None,
            fail: true,
        }
        .spawn()
        .on_system(&sys)
        .detached()
        .await?;
        handle.wait_stopped().await;
        assert!(matches!(handle.send(()).await, Err(AskError::Closed)));
        Ok(())
    });
}

/// README: "### ActorConfig Builder".
#[test]
#[rustfmt::skip]
fn api_at_a_glance_actor_config_builder() {
    run_block(async {
        let config = ActorConfig::default()
            .with_mailbox_capacity(512)
            .with_priority_lane(16)               // Enable a priority lane; no default capacity
            .supervisor()                         // OneForOne, 3 restarts / 5s
            .with_supervision(SupervisionConfig::one_for_all()
                .max_restarts(10, Duration::from_secs(60)));  // Custom strategy + budget

        let sys = ActorSystem::create("readme-config-builder")?;
        let handle = MyActor.spawn().with_config(config).on_system(&sys).await?;
        assert_eq!(handle.mailbox_capacity(), 512);
        assert_eq!(handle.priority_capacity(), 16);
        handle.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// ===========================================================================
// Patterns table
// ===========================================================================
//
// Rows pinned by an existing test instead of one here:
//
// - "Kill latency, or turn atomicity", the `Duration::ZERO` half:
//   `tests/kill_grace.rs::zero_kill_grace_aborts_at_once_and_is_not_a_verdict`.
// - "A trapping worker under a killed supervisor":
//   `tests/cascade.rs::inherited_stop_with_an_edge_of_range_shutdown` (the
//   killed root runs no hook, the supervisor under it runs `on_stopped` with
//   `ParentKilled`, and the worker under that runs its own with
//   `ParentRequest`).

/// A turn that can be held open: it signals `entered`, waits for `release`,
/// then sets `finished`.
fn held(entered: &Arc<Notify>, release: &Arc<Notify>, finished: &Arc<AtomicBool>) -> Held {
    Held {
        entered: entered.clone(),
        release: release.clone(),
        finished: finished.clone(),
    }
}

/// A peer that answers only after it is released.
struct Gated {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for Gated {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, msg: u32, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(msg)
    }
}

// --- spawn_request ----------------------------------------------------------

enum AskerMsg {
    Request,
    Spawned(Result<ActorHandle<SlowStart>, SpawnError>),
    Count,
}

struct Asker {
    sys: Arc<ActorSystem>,
    entered: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
    spawned: usize,
    done: mpsc::UnboundedSender<()>,
}

impl Actor for Asker {
    type Message = AskerMsg;
    type Response = usize;

    async fn handle(&mut self, msg: AskerMsg, ctx: &mut ActorContext<Self>) -> ActorResult<usize> {
        match msg {
            AskerMsg::Request => {
                let me = ctx.self_handle();
                let spawn = SlowStart {
                    entered: self.entered.take(),
                    release: self.release.take(),
                    fail: false,
                }
                .spawn()
                .on_system(&self.sys);
                // The spawn future is `Send`: it runs on its own task and the
                // result comes back as a message.
                tokio::spawn(async move {
                    let result = spawn.await;
                    let _ = me.notify(AskerMsg::Spawned(result)).await;
                });
                Ok(0)
            }
            AskerMsg::Spawned(result) => {
                result.expect("the asked-for actor starts");
                self.spawned += 1;
                let _ = self.done.send(());
                Ok(self.spawned)
            }
            AskerMsg::Count => Ok(self.spawned),
        }
    }
}

/// README, Patterns: `spawn_request`.
#[test]
fn patterns_spawn_request() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-spawn-request")?;
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (done, mut done_rx) = mpsc::unbounded_channel();
        let asker = Asker {
            sys: sys.clone(),
            entered: Some(entered_tx),
            release: Some(release_rx),
            spawned: 0,
            done,
        }
        .spawn()
        .on_system(&sys)
        .await?;

        asker.send(AskerMsg::Request).await?;
        // The asked-for actor is inside its `pre_start`, and the asker still
        // answers: its handler did not wait for the spawn.
        entered_rx.await?;
        assert_eq!(asker.send(AskerMsg::Count).await?, 0);
        release_tx.send(()).unwrap();
        done_rx
            .recv()
            .await
            .expect("the spawn result arrives as a message");
        assert_eq!(asker.send(AskerMsg::Count).await?, 1);
        asker.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// --- is_process_alive after exit(Pid, kill) -----------------------------------

/// README, Patterns: `is_process_alive` after `exit(Pid, kill)`.
#[test]
fn patterns_is_alive_after_a_kill() {
    run_paused_block(async {
        let sys = ActorSystem::create("readme-pattern-is-alive")?;

        // `is_alive()` reads true right after `stop(Kill)` returns, until the
        // actor reaches its next turn boundary: its turn is held and its grace
        // is long.
        let (entered, release, finished) = (
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        );
        let h = held(&entered, &release, &finished)
            .spawn()
            .kill_grace(Duration::from_secs(3600))
            .on_system(&sys)
            .await?;
        h.notify(()).await?;
        entered.notified().await;
        h.stop(StopReason::Kill).await?;
        assert!(h.is_alive());
        release.notify_one();
        assert_eq!(h.wait_stopped().await, StopReason::Kill);
        assert!(!h.is_alive());

        // The awaited forms return once the end is reached.
        let new_held = || async {
            let h = held(&entered, &release, &finished)
                .spawn()
                .on_system(&sys)
                .await
                .unwrap();
            h.notify(()).await.unwrap();
            entered.notified().await;
            h
        };

        let h = new_held().await;
        let report = sys.kill_by_id(h.id()).await.expect("a registered id");
        assert_eq!(report.reason, Some(StopReason::Kill));
        assert!(!h.is_alive());

        let h = new_held().await;
        let outcome = h
            .stop_within(StopReason::Kill, Duration::from_millis(50))
            .await?;
        assert!(matches!(
            outcome,
            StopOutcome::Killed | StopOutcome::Aborted
        ));
        assert!(!h.is_alive());
        Ok(())
    });
}

// --- via registries -----------------------------------------------------------

/// README, Patterns: `via` registries.
#[test]
fn patterns_via_registries() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-via")?;
        // Your own key -> ActorId map; the actors are anonymous.
        let mut registry: std::collections::HashMap<(&str, u32), tokio_actors::ActorId> =
            std::collections::HashMap::new();
        let a = Summer::default().spawn().on_system(&sys).await?;
        let b = Summer::default().spawn().on_system(&sys).await?;
        registry.insert(("player", 1), a.id().clone());
        registry.insert(("player", 2), b.id().clone());

        let resolved = sys
            .get_by_id::<Summer>(&registry[&("player", 2)])
            .expect("the key resolves");
        assert_eq!(resolved, b);
        assert_eq!(resolved.send(4).await?, 4);
        assert_eq!(a.send(1).await?, 1);

        // A stopped actor no longer resolves.
        b.stop(StopReason::Graceful).await?;
        b.wait_stopped().await;
        wait_until("the id to leave the registry", || {
            sys.get_by_id::<Summer>(&registry[&("player", 2)]).is_none()
        })
        .await;
        a.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// --- timer:send_after, timer:kill_after ---------------------------------------

/// README, Patterns: `timer:send_after` to another process and
/// `timer:kill_after`, on a paused clock.
#[test]
fn patterns_timer_send_after_and_kill_after() {
    common::run_paused(async {
        let sys = ActorSystem::create("readme-pattern-send-after").unwrap();
        let a = Summer::default().spawn().on_system(&sys).await.unwrap();

        // send_after: sleep, then notify.
        let target = a.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            target.notify(3).await.unwrap();
        })
        .await
        .unwrap();
        assert_eq!(a.send(0).await.unwrap(), 3);

        // stop after: sleep, then stop.
        let target = a.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            target.stop(StopReason::Graceful).await.unwrap();
        })
        .await
        .unwrap();
        assert_eq!(a.wait_stopped().await, StopReason::Graceful);

        // A send to an actor that has stopped returns Closed.
        assert!(matches!(
            a.notify(1).await,
            Err(tokio_actors::SendError::Closed)
        ));

        // kill_after: sleep, then kill_by_id.
        let b = Summer::default().spawn().on_system(&sys).await.unwrap();
        let (id, system) = (b.id().clone(), sys.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            system.kill_by_id(&id).await;
        })
        .await
        .unwrap();
        assert_eq!(b.wait_stopped().await, StopReason::Kill);
    });
}

// --- timer:read_timer ---------------------------------------------------------

enum ReadTimer {
    Arm(tokio::time::Instant),
    Fire,
}

struct Armed {
    fired: mpsc::UnboundedSender<()>,
}

impl Actor for Armed {
    type Message = ReadTimer;
    type Response = ();

    async fn handle(&mut self, msg: ReadTimer, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ReadTimer::Arm(when) => {
                ctx.schedule(ReadTimer::Fire).at(when).await?;
            }
            ReadTimer::Fire => {
                let _ = self.fired.send(());
            }
        }
        Ok(())
    }
}

/// README, Patterns: `timer:read_timer`, on a paused clock.
#[test]
fn patterns_timer_read_timer() {
    common::run_paused(async {
        use tokio::time::Instant;

        let (fired, mut seen) = mpsc::unbounded_channel();
        let sys = ActorSystem::create("readme-pattern-read-timer").unwrap();
        let actor = Armed { fired }.spawn().on_system(&sys).await.unwrap();

        // Keep the Instant passed to `at` ...
        let when = Instant::now() + Duration::from_secs(60);
        actor.send(ReadTimer::Arm(when)).await.unwrap();
        // ... and subtract Instant::now().
        let remaining = || when.saturating_duration_since(Instant::now());
        assert_eq!(remaining(), Duration::from_secs(60));
        tokio::time::advance(Duration::from_secs(20)).await;
        assert_eq!(remaining(), Duration::from_secs(40));
        assert!(seen.try_recv().is_err(), "the timer has not fired yet");
        tokio::time::advance(Duration::from_secs(40)).await;
        seen.recv().await.expect("the timer fires at the Instant");
        assert_eq!(remaining(), Duration::ZERO);
        actor.stop(StopReason::Graceful).await.unwrap();
    });
}

// --- the process dictionary ---------------------------------------------------

/// README, Patterns: the process dictionary. State the actor keeps in its own
/// typed fields lives across messages.
#[test]
fn patterns_the_process_dictionary() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-dictionary")?;
        let actor = Summer::default().spawn().on_system(&sys).await?;
        assert_eq!(actor.send(2).await?, 2);
        assert_eq!(actor.send(3).await?, 5);
        actor.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// --- a deferred reply ---------------------------------------------------------

enum Deferred {
    Ask(oneshot::Sender<u32>),
    Answer(u32),
}

#[derive(Default)]
struct Deferrer {
    waiting: Vec<oneshot::Sender<u32>>,
}

impl Actor for Deferrer {
    type Message = Deferred;
    type Response = ();

    async fn handle(&mut self, msg: Deferred, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Deferred::Ask(reply) => self.waiting.push(reply),
            Deferred::Answer(value) => {
                for reply in self.waiting.drain(..) {
                    let _ = reply.send(value);
                }
            }
        }
        Ok(())
    }
}

/// README, Patterns: a deferred reply.
#[test]
fn patterns_a_deferred_reply() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-deferred")?;
        let actor = Deferrer::default().spawn().on_system(&sys).await?;
        let (tx, mut rx) = oneshot::channel();
        // The turn that took the sender is over, the reply is not sent.
        actor.send(Deferred::Ask(tx)).await?;
        assert!(rx.try_recv().is_err());
        // A later turn answers it.
        actor.send(Deferred::Answer(9)).await?;
        assert_eq!(rx.await?, 9);
        actor.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// --- send_request, without blocking -------------------------------------------

enum Requester {
    Ask(u32),
    Reply(StreamEvent<Result<u32, AskError>>),
    Idle,
}

impl From<StreamEvent<Result<u32, AskError>>> for Requester {
    fn from(event: StreamEvent<Result<u32, AskError>>) -> Self {
        Requester::Reply(event)
    }
}

struct Asking {
    peer: ActorHandle<Gated>,
    replies: mpsc::UnboundedSender<u32>,
}

impl Actor for Asking {
    type Message = Requester;
    type Response = ();

    async fn handle(&mut self, msg: Requester, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        use tokio_stream::StreamExt;

        match msg {
            Requester::Ask(msg) => {
                // The README builds the one-item stream with
                // `futures::stream::once`; `tokio_stream` gives the same shape.
                let h = self.peer.clone();
                ctx.add_stream(Box::pin(tokio_stream::once(msg).then(move |msg| {
                    let h = h.clone();
                    async move { h.send(msg).await }
                })));
            }
            Requester::Reply(StreamEvent::Data(reply)) => {
                let _ = self.replies.send(reply.expect("the peer answers"));
            }
            Requester::Reply(StreamEvent::Finished) | Requester::Idle => {}
        }
        Ok(())
    }
}

/// README, Patterns: `send_request`, without blocking.
#[test]
fn patterns_send_request_without_blocking() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-send-request")?;
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let peer = Gated {
            entered: entered.clone(),
            release: release.clone(),
        }
        .spawn()
        .on_system(&sys)
        .await?;
        let (replies, mut seen) = mpsc::unbounded_channel();
        let asker = Asking {
            peer: peer.clone(),
            replies,
        }
        .spawn()
        .on_system(&sys)
        .await?;

        asker.send(Requester::Ask(5)).await?;
        // The peer holds the request, and the asker's turn is free.
        entered.notified().await;
        asker.send(Requester::Idle).await?;
        assert!(seen.try_recv().is_err());
        release.notify_one();
        assert_eq!(seen.recv().await, Some(5));
        asker.stop(StopReason::Graceful).await?;
        peer.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// --- a dedicated supervisor process -------------------------------------------

/// A supervisor with an empty `handle` and two children: one whose turn can be
/// held and one that can crash.
struct Dedicated {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    finished: Arc<AtomicBool>,
}

impl Actor for Dedicated {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (entered, release, finished) = (
            self.entered.clone(),
            self.release.clone(),
            self.finished.clone(),
        );
        ctx.spawn_child(move || held(&entered, &release, &finished))
            .named("busy")
            .await?;
        ctx.spawn_child(Worker::default).named("crashy").await?;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A supervisor whose own `handle` can be held open, with one crashable child.
struct Occupied {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for Occupied {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(Worker::default).named("crashy").await?;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

async fn crash_and_see_it_stop(sys: &ActorSystem, name: &str) -> ActorHandle<Worker> {
    let crashed = sys.get::<Worker>(name).expect("the child is registered");
    assert!(crashed.send(WorkerMsg::Crash).await.is_err());
    crashed.wait_stopped().await;
    crashed
}

async fn running_again(sys: &ActorSystem, name: &str) {
    wait_until("the child to run again", || {
        sys.get::<Worker>(name)
            .is_some_and(|w| w.status() == ActorStatus::Running)
    })
    .await;
}

/// README, Patterns: a dedicated supervisor process. Its supervision events
/// never wait behind a long user turn, where a supervisor that also does work
/// handles them after its turn. The paused clock makes "has not happened"
/// observable: the runtime is quiet when the clock advances.
#[test]
fn patterns_a_dedicated_supervisor_process() {
    common::run_paused(async {
        let sys = ActorSystem::create("readme-pattern-dedicated").unwrap();

        // The dedicated supervisor: a sibling's long turn does not delay the
        // restart of a crashed child.
        let (entered, release, finished) = (
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        );
        let sup = Dedicated {
            entered: entered.clone(),
            release: release.clone(),
            finished: finished.clone(),
        }
        .spawn()
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        let busy = sys.get::<Held>("busy").expect("the busy child");
        busy.notify(()).await.unwrap();
        entered.notified().await;
        crash_and_see_it_stop(&sys, "crashy").await;
        running_again(&sys, "crashy").await;
        release.notify_one();
        sup.stop(StopReason::Graceful).await.unwrap();
        sup.wait_stopped().await;

        // A supervisor that is in the middle of a long turn of its own
        // restarts the child only after the turn.
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let busy_sup = Occupied {
            entered: entered.clone(),
            release: release.clone(),
        }
        .spawn()
        .named("occupied")
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        // The earlier supervisor's child name is free again.
        busy_sup.notify(()).await.unwrap();
        entered.notified().await;
        let crashed = crash_and_see_it_stop(&sys, "crashy").await;
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert!(
            !crashed.is_alive()
                && sys
                    .get::<Worker>("crashy")
                    .is_none_or(|w| w.status() != ActorStatus::Running),
            "the restart waited for the supervisor's turn"
        );
        release.notify_one();
        running_again(&sys, "crashy").await;
        busy_sup.stop(StopReason::Graceful).await.unwrap();
    });
}

// --- an ordered initial start; a strict start ---------------------------------

type StartLog = Arc<Mutex<Vec<String>>>;

/// A child that logs each step of its start and its stop.
struct Logging {
    name: &'static str,
    log: StartLog,
    fail_start: bool,
}

impl Logging {
    fn note(&self, step: &str) {
        self.log
            .lock()
            .unwrap()
            .push(format!("{} {step}", self.name));
    }
}

impl Actor for Logging {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.note("pre_start");
        if self.fail_start {
            return Err(ActorError::user("start failed"));
        }
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.note("on_started");
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _r: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.note("on_stopped");
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Spawns its children in order. `strict` maps a failed `get_status()` to the
/// supervisor's own failure with `?`, as the strict-start row does.
struct Starter {
    log: StartLog,
    plan: Vec<(&'static str, bool)>,
    strict: bool,
    first: Arc<Mutex<Option<ActorHandle<Logging>>>>,
}

impl Actor for Starter {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for (name, fail_start) in self.plan.clone() {
            let log = self.log.clone();
            let child = ctx
                .spawn_child(move || Logging {
                    name,
                    log: log.clone(),
                    fail_start,
                })
                .named(name)
                .await?;
            self.first
                .lock()
                .unwrap()
                .get_or_insert_with(|| child.clone());
            if self.strict {
                child
                    .get_status()
                    .await
                    .map_err(|e| ActorError::user(e.to_string()))?;
            } else {
                let _ = child.get_status().await;
            }
        }
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// README, Patterns: an ordered initial start.
#[test]
fn patterns_an_ordered_initial_start() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-ordered-start")?;
        let log: StartLog = Arc::default();
        let sup = Starter {
            log: log.clone(),
            plan: vec![("a", false), ("b", false), ("c", false)],
            strict: false,
            first: Arc::default(),
        }
        .spawn()
        .on_system(&sys)
        .supervisor()
        .await?;
        // Each child finished its `pre_start` and `on_started` before the next
        // one was spawned.
        assert_eq!(
            *log.lock().unwrap(),
            [
                "a pre_start",
                "a on_started",
                "b pre_start",
                "b on_started",
                "c pre_start",
                "c on_started"
            ]
        );
        sup.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

/// A child that calls back into its supervisor while it initializes.
struct CallsBack {
    supervisor: Arc<Mutex<Option<ActorHandle<Calling>>>>,
}

impl Actor for CallsBack {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let supervisor = self.supervisor.lock().unwrap().clone();
        if let Some(supervisor) = supervisor {
            let _ = supervisor.send(()).await;
        }
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

struct Calling {
    slot: Arc<Mutex<Option<ActorHandle<Calling>>>>,
}

impl Actor for Calling {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        *self.slot.lock().unwrap() = Some(ctx.self_handle());
        let slot = self.slot.clone();
        let child = ctx
            .spawn_child(move || CallsBack {
                supervisor: slot.clone(),
            })
            .await?;
        child
            .get_status()
            .await
            .map_err(|e| ActorError::user(e.to_string()))?;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// README, Patterns: an ordered initial start, the last clause: a child that
/// calls back into the supervisor during its initialization waits forever, so
/// only a `start_timeout` ends the supervisor's start. The paused clock turns
/// an hour of waiting into an observation.
#[test]
fn patterns_an_ordered_initial_start_waits_forever_on_a_callback() {
    common::run_paused(async {
        let sys = ActorSystem::create("readme-pattern-callback").unwrap();
        let slot = Arc::new(Mutex::new(None));

        // No bound: still pending after an hour.
        let spawn = Calling { slot: slot.clone() }
            .spawn()
            .named("unbounded")
            .on_system(&sys)
            .supervisor();
        assert!(
            tokio::time::timeout(Duration::from_secs(3600), spawn)
                .await
                .is_err(),
            "the start must not complete"
        );

        // With a bound: the start ends with `StartTimeout`.
        let result = Calling {
            slot: Arc::new(Mutex::new(None)),
        }
        .spawn()
        .named("bounded")
        .on_system(&sys)
        .supervisor()
        .start_timeout(Duration::from_secs(1))
        .await;
        assert!(
            matches!(result, Err(SpawnError::StartTimeout)),
            "{:?}",
            result.err()
        );
    });
}

/// README, Patterns: a strict start.
#[test]
fn patterns_a_strict_start() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-strict-start")?;
        let log: StartLog = Arc::default();
        let first = Arc::default();
        let result = Starter {
            log: log.clone(),
            plan: vec![("good", false), ("bad", true), ("never", false)],
            strict: true,
            first: Arc::clone(&first),
        }
        .spawn()
        .on_system(&sys)
        .supervisor()
        .await;
        assert!(
            matches!(result, Err(SpawnError::Init(_))),
            "{:?}",
            result.err()
        );

        // The child started before the failure was stopped by the teardown;
        // the one after it never started.
        let good: ActorHandle<Logging> = first.lock().unwrap().clone().expect("the first child");
        good.wait_stopped().await;
        let log = log.lock().unwrap().clone();
        assert!(log.contains(&"good on_started".to_string()), "{log:?}");
        assert!(log.contains(&"good on_stopped".to_string()), "{log:?}");
        assert!(!log.iter().any(|line| line.starts_with("never")), "{log:?}");
        Ok(())
    });
}

// --- supervisor:count_children, supervisor:which_children ---------------------

enum Census {
    Count,
    Alive,
    Has(&'static str),
}

struct Registrar;

impl Actor for Registrar {
    type Message = Census;
    type Response = usize;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(Worker::default).named("one").await?;
        ctx.spawn_child(Worker::default)
            .named("two")
            .restart_type(tokio_actors::RestartType::Temporary)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, msg: Census, ctx: &mut ActorContext<Self>) -> ActorResult<usize> {
        Ok(match msg {
            Census::Count => ctx.children().len(),
            Census::Alive => ctx.children().iter().filter(|c| c.is_alive).count(),
            Census::Has(name) => usize::from(
                ctx.children()
                    .iter()
                    .any(|c| c.name.as_deref() == Some(name)),
            ),
        })
    }
}

/// README, Patterns: `supervisor:count_children`, `supervisor:which_children`.
#[test]
fn patterns_count_children_and_which_children() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-count-children")?;
        let sup = Registrar.spawn().on_system(&sys).supervisor().await?;
        assert_eq!(sup.send(Census::Count).await?, 2);
        assert_eq!(sup.send(Census::Has("two")).await?, 1);
        assert_eq!(sup.send(Census::Has("three")).await?, 0);

        // A Temporary child that ends is gone from the list.
        let two = sys.get::<Worker>("two").unwrap();
        let _ = two.send(WorkerMsg::Crash).await;
        two.wait_stopped().await;
        // The supervisor drops the spec when it handles the death.
        tokio::time::timeout(Duration::from_secs(30), async {
            while sup.send(Census::Count).await.unwrap() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the temporary child to leave the list");
        assert_eq!(sup.send(Census::Alive).await?, 1);
        sup.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// --- supervisor_bridge --------------------------------------------------------

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct Bridge {
    release: Option<oneshot::Receiver<()>>,
    dropped: Arc<AtomicBool>,
    abort: Option<tokio::task::AbortHandle>,
}

enum BridgeMsg {
    Ended(StreamEvent<Result<(), tokio::task::JoinError>>),
}

impl From<StreamEvent<Result<(), tokio::task::JoinError>>> for BridgeMsg {
    fn from(event: StreamEvent<Result<(), tokio::task::JoinError>>) -> Self {
        BridgeMsg::Ended(event)
    }
}

impl Actor for Bridge {
    type Message = BridgeMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        use tokio_stream::StreamExt;

        let release = self.release.take().expect("started once");
        let flag = DropFlag(self.dropped.clone());
        let task = tokio::spawn(async move {
            let _flag = flag;
            let _ = release.await;
        });
        self.abort = Some(task.abort_handle());
        // The task's `JoinHandle` as a one-item stream.
        ctx.add_stream(Box::pin(tokio_stream::once(task).then(|task| task)));
        Ok(())
    }

    async fn handle(&mut self, msg: BridgeMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            BridgeMsg::Ended(StreamEvent::Data(end)) => {
                // The task's end becomes the actor's failure.
                Err(ActorError::user(format!("the bridged task ended: {end:?}")))
            }
            BridgeMsg::Ended(StreamEvent::Finished) => Ok(()),
        }
    }

    async fn on_stopped(
        &mut self,
        _r: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        Ok(())
    }
}

/// README, Patterns: `supervisor_bridge`.
#[test]
fn patterns_supervisor_bridge() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-bridge")?;

        // The task ends by itself: the bridge fails with that end.
        let (release, release_rx) = oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let bridge = Bridge {
            release: Some(release_rx),
            dropped: dropped.clone(),
            abort: None,
        }
        .spawn()
        .on_system(&sys)
        .await?;
        release.send(()).unwrap();
        assert!(matches!(
            bridge.wait_stopped().await,
            StopReason::Failure(ActorError::User(text)) if text.contains("the bridged task ended")
        ));

        // The bridge stops first: `on_stopped` aborts the task.
        let (_keep, release_rx) = oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let bridge = Bridge {
            release: Some(release_rx),
            dropped: dropped.clone(),
            abort: None,
        }
        .spawn()
        .on_system(&sys)
        .await?;
        assert!(!dropped.load(Ordering::SeqCst));
        bridge.stop(StopReason::Graceful).await?;
        bridge.wait_stopped().await;
        wait_until("the bridged task to be aborted", || {
            dropped.load(Ordering::SeqCst)
        })
        .await;
        Ok(())
    });
}

// --- pipelined replies (become) -----------------------------------------------

enum Pipeline {
    Start,
    Done(u32),
    State,
}

struct Pipelined {
    release: Option<oneshot::Receiver<u32>>,
    result: Option<u32>,
    pending: bool,
    handled_result: mpsc::UnboundedSender<()>,
}

impl Actor for Pipelined {
    type Message = Pipeline;
    type Response = (bool, Option<u32>);

    async fn handle(
        &mut self,
        msg: Pipeline,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<(bool, Option<u32>)> {
        match msg {
            Pipeline::Start => {
                // Update state, hand the rest of the work to a task, and take
                // its result later as a message.
                self.pending = true;
                let release = self.release.take().expect("started once");
                let me = ctx.self_handle();
                tokio::spawn(async move {
                    let value = release.await.unwrap();
                    let _ = me.notify(Pipeline::Done(value)).await;
                });
            }
            Pipeline::Done(value) => {
                self.pending = false;
                self.result = Some(value);
                let _ = self.handled_result.send(());
            }
            Pipeline::State => {}
        }
        Ok((self.pending, self.result))
    }
}

/// README, Patterns: pipelined replies (`become`).
#[test]
fn patterns_pipelined_replies() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-pipelined")?;
        let (release, release_rx) = oneshot::channel();
        let (handled_result, mut handled) = mpsc::unbounded_channel();
        let actor = Pipelined {
            release: Some(release_rx),
            result: None,
            pending: false,
            handled_result,
        }
        .spawn()
        .on_system(&sys)
        .await?;
        assert_eq!(actor.send(Pipeline::Start).await?, (true, None));
        // The work is outstanding and the actor already answers other mail.
        assert_eq!(actor.send(Pipeline::State).await?, (true, None));
        release.send(42).unwrap();
        handled
            .recv()
            .await
            .expect("the result arrives as a message");
        assert_eq!(actor.send(Pipeline::State).await?, (false, Some(42)));
        actor.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// --- kill latency, or turn atomicity -------------------------------------------

/// README, Patterns: kill latency, or turn atomicity, the other half: a grace
/// beyond any practical bound keeps every turn whole, and makes every wait on
/// that actor's `Kill`, its parent's stop budget included, as unbounded as
/// under `Shutdown::Infinity`. The paused clock lets an hour pass.
#[test]
fn patterns_a_long_kill_grace_keeps_every_turn_whole() {
    common::run_paused(async {
        let sys = ActorSystem::create("readme-pattern-long-grace").unwrap();
        let (entered, release, finished) = (
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        );
        let ten_years = Duration::from_secs(10 * 365 * 86_400);

        // The Kill waits for the turn boundary, however long the turn is held.
        let h = held(&entered, &release, &finished)
            .spawn()
            .kill_grace(ten_years)
            .on_system(&sys)
            .await
            .unwrap();
        h.notify(()).await.unwrap();
        entered.notified().await;
        h.stop(StopReason::Kill).await.unwrap();
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert!(h.is_alive() && !finished.load(Ordering::SeqCst));
        release.notify_one();
        assert_eq!(h.wait_stopped().await, StopReason::Kill);
        assert!(finished.load(Ordering::SeqCst), "the turn ran whole");

        // The parent's stop budget waits on the child's Kill as it would
        // under Infinity.
        struct Parent {
            entered: Arc<Notify>,
            release: Arc<Notify>,
            finished: Arc<AtomicBool>,
        }
        impl Actor for Parent {
            type Message = ();
            type Response = ();
            async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
                let (entered, release, finished) = (
                    self.entered.clone(),
                    self.release.clone(),
                    self.finished.clone(),
                );
                ctx.spawn_child(move || held(&entered, &release, &finished))
                    .named("held")
                    .kill_grace(Duration::from_secs(10 * 365 * 86_400))
                    .shutdown(Shutdown::Timeout(Duration::from_millis(100)))
                    .await?;
                Ok(())
            }
            async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
                Ok(())
            }
        }
        finished.store(false, Ordering::SeqCst);
        let parent = Parent {
            entered: entered.clone(),
            release: release.clone(),
            finished: finished.clone(),
        }
        .spawn()
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        let child = sys.get::<Held>("held").unwrap();
        child.notify(()).await.unwrap();
        entered.notified().await;
        parent.stop(StopReason::Graceful).await.unwrap();
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert_ne!(
            parent.status(),
            ActorStatus::Stopped,
            "the parent still waits"
        );
        release.notify_one();
        parent.wait_stopped().await;
        assert!(finished.load(Ordering::SeqCst));
    });
}

// --- locality -----------------------------------------------------------------

struct Locator {
    system: String,
}

impl Actor for Locator {
    type Message = &'static str;
    type Response = u32;

    async fn handle(
        &mut self,
        msg: &'static str,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<u32> {
        // It holds no handle: it reaches the others through its system.
        let sys = ActorSystem::get_named(&self.system).expect("its system");
        match msg {
            "get" => {
                let peer = sys.get::<MyActor>("peer").expect("registered by name");
                peer.send(11)
                    .await
                    .map_err(|e| ActorError::user(e.to_string()))
            }
            "get_by_id" => {
                let id = sys
                    .actor_ids()
                    .into_iter()
                    .find(|id| id.as_str() == "peer")
                    .expect("enumerable");
                let peer = sys.get_by_id::<MyActor>(&id).expect("registered by id");
                peer.send(12)
                    .await
                    .map_err(|e| ActorError::user(e.to_string()))
            }
            _ => {
                let report = sys.kill_by_id(&"peer".into()).await;
                Ok(u32::from(report.is_some()))
            }
        }
    }
}

/// README, Patterns: locality.
#[test]
fn patterns_locality() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-locality")?;
        let peer = MyActor.spawn().named("peer").on_system(&sys).await?;
        let locator = Locator {
            system: sys.name().to_string(),
        }
        .spawn()
        .on_system(&sys)
        .await?;
        assert_eq!(locator.send("get").await?, 11);
        assert_eq!(locator.send("get_by_id").await?, 12);
        assert_eq!(locator.send("kill_by_id").await?, 1);
        assert_eq!(peer.wait_stopped().await, StopReason::Kill);
        Ok(())
    });
}

// --- address identity ---------------------------------------------------------

/// README, Patterns: address identity.
#[test]
fn patterns_address_identity() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-address")?;
        let sup = Registrar.spawn().on_system(&sys).supervisor().await?;
        let old = sys.get::<Worker>("one").expect("the child");
        let _ = old.send(WorkerMsg::Crash).await;
        // The name is free once the crash is read: a lookup finds nothing yet
        // or the successor, which answers; the handle in hand belongs to the
        // ended incarnation.
        if let Some(next) = sys.get::<Worker>("one") {
            next.send(WorkerMsg::Ping).await?;
        }
        assert!(matches!(
            old.send(WorkerMsg::Ping).await,
            Err(AskError::Closed)
        ));
        old.wait_stopped().await;
        wait_until("the successor", || {
            sys.get::<Worker>("one")
                .is_some_and(|w| w.status() == ActorStatus::Running)
        })
        .await;

        // A handle to the ended incarnation and one to its successor compare
        // equal, and only the successor is alive.
        let successor = sys.get::<Worker>("one").unwrap();
        assert!(old == successor);
        assert!(!old.is_alive());
        assert!(successor.is_alive());
        assert!(matches!(
            old.send(WorkerMsg::Ping).await,
            Err(AskError::Closed)
        ));
        // Resolving the id again gives the live one.
        let again = sys.get_by_id::<Worker>(old.id()).expect("resolves");
        again.send(WorkerMsg::Ping).await?;
        sup.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// --- postpone, stash ----------------------------------------------------------

enum Stash {
    Work(u32),
    Unlock,
    Done,
}

struct Stashing {
    locked: bool,
    stash: Vec<u32>,
    done: Vec<u32>,
}

impl Stashing {
    const CAP: usize = 2;
}

impl Actor for Stashing {
    type Message = Stash;
    type Response = Vec<u32>;

    async fn handle(&mut self, msg: Stash, _ctx: &mut ActorContext<Self>) -> ActorResult<Vec<u32>> {
        match msg {
            Stash::Work(n) if self.locked => {
                // Capped by the actor itself: the queue sits outside the mailbox bound.
                if self.stash.len() < Self::CAP {
                    self.stash.push(n);
                }
            }
            Stash::Work(n) => self.done.push(n),
            Stash::Unlock => {
                self.locked = false;
                self.done.append(&mut self.stash);
            }
            Stash::Done => {}
        }
        Ok(self.done.clone())
    }
}

/// README, Patterns: postpone, stash.
#[test]
fn patterns_postpone_and_stash() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-stash")?;
        let actor = Stashing {
            locked: true,
            stash: Vec::new(),
            done: Vec::new(),
        }
        .spawn()
        .on_system(&sys)
        .await?;
        for n in 1..=3 {
            actor.notify(Stash::Work(n)).await?;
        }
        assert!(actor.send(Stash::Done).await?.is_empty(), "postponed");
        // Replayed in arrival order once the state allows; the stash held two.
        assert_eq!(actor.send(Stash::Unlock).await?, vec![1, 2]);
        assert_eq!(actor.send(Stash::Work(4)).await?, vec![1, 2, 4]);
        actor.stop(StopReason::Graceful).await?;
        Ok(())
    });
}

// --- delivery at the end ------------------------------------------------------

struct Ending {
    handled: Arc<AtomicUsize>,
    in_on_stopped: Arc<Notify>,
    release: Arc<Notify>,
}

impl Actor for Ending {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, msg: u32, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        self.handled.fetch_add(1, Ordering::SeqCst);
        Ok(msg)
    }

    async fn on_stopped(
        &mut self,
        _r: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.in_on_stopped.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

/// README, Patterns: delivery at the end. A message accepted while the actor
/// runs `on_stopped` is dropped unhandled, and from the return of `on_stopped`
/// on, sends are refused.
#[test]
fn patterns_delivery_at_the_end() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-delivery-end")?;
        let (handled, in_on_stopped, release) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
        );
        let actor = Ending {
            handled: handled.clone(),
            in_on_stopped: in_on_stopped.clone(),
            release: release.clone(),
        }
        .spawn()
        .on_system(&sys)
        .await?;
        assert_eq!(actor.send(1).await?, 1);

        actor.stop(StopReason::Graceful).await?;
        in_on_stopped.notified().await;
        // Accepted while `on_stopped` runs ...
        actor.notify(2).await?;
        let asked = tokio::spawn({
            let actor = actor.clone();
            async move { actor.send(3).await }
        });
        release.notify_one();
        assert_eq!(actor.wait_stopped().await, StopReason::Graceful);
        // ... and dropped unhandled: the asker is answered with an error.
        assert!(asked.await?.is_err());
        assert_eq!(handled.load(Ordering::SeqCst), 1);
        // From the return of `on_stopped` on, sends are refused.
        assert!(matches!(
            actor.notify(4).await,
            Err(tokio_actors::SendError::Closed)
        ));
        assert!(matches!(actor.send(5).await, Err(AskError::Closed)));
        Ok(())
    });
}

// --- a send that never blocks -------------------------------------------------

/// README, Patterns: a send that never blocks.
#[test]
fn patterns_a_send_that_never_blocks() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-never-blocks")?;
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let gate = || Gated {
            entered: entered.clone(),
            release: release.clone(),
        };

        // With the default bound the third notify waits for room behind a
        // held turn: one in the turn, then the queue.
        let bounded = gate()
            .spawn()
            .with_config(ActorConfig::default().with_mailbox_capacity(1))
            .on_system(&sys)
            .await?;
        bounded.notify(0).await?;
        entered.notified().await;
        bounded.notify(1).await?;
        let waited = tokio::time::timeout(Duration::ZERO, bounded.notify(2)).await;
        assert!(waited.is_err(), "a full mailbox makes notify wait");
        assert!(matches!(bounded.try_notify(3), Err(TrySendError::Full)));

        // `usize::MAX` is clamped to the semaphore bound and never fills.
        let unbounded = gate()
            .spawn()
            .with_config(ActorConfig::default().with_mailbox_capacity(usize::MAX))
            .on_system(&sys)
            .await?;
        assert_eq!(
            unbounded.mailbox_capacity(),
            tokio::sync::Semaphore::MAX_PERMITS
        );
        unbounded.notify(0).await?;
        entered.notified().await;
        for n in 0..1_000 {
            let sent = tokio::time::timeout(Duration::ZERO, unbounded.notify(n)).await;
            assert!(matches!(sent, Ok(Ok(()))), "notify {n} waited");
        }
        assert_eq!(unbounded.mailbox_len(), 1_000);
        release.notify_waiters();
        for h in [&bounded, &unbounded] {
            h.stop(StopReason::Kill).await?;
        }
        Ok(())
    });
}

// --- the gen_server:stop order ------------------------------------------------

enum Orderly {
    Work(u32),
    Stop,
}

struct Orderer {
    handled: Arc<Mutex<Vec<u32>>>,
}

impl Actor for Orderer {
    type Message = Orderly;
    type Response = ();

    async fn handle(&mut self, msg: Orderly, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Orderly::Work(n) => self.handled.lock().unwrap().push(n),
            Orderly::Stop => {
                ctx.self_handle().stop(StopReason::Graceful).await.unwrap();
            }
        }
        Ok(())
    }
}

/// README, Patterns: the `gen_server:stop` order. Mail queued before the
/// `Stop` message is handled first.
#[test]
fn patterns_the_gen_server_stop_order() {
    run_block(async {
        let sys = ActorSystem::create("readme-pattern-stop-order")?;
        let handled = Arc::new(Mutex::new(Vec::new()));
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        // A held first message lets the rest queue up in order.
        struct Front {
            inner: Orderer,
            entered: Arc<Notify>,
            release: Arc<Notify>,
        }
        impl Actor for Front {
            type Message = Orderly;
            type Response = ();
            async fn handle(
                &mut self,
                msg: Orderly,
                ctx: &mut ActorContext<Self>,
            ) -> ActorResult<()> {
                if let Orderly::Work(0) = msg {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                match msg {
                    Orderly::Work(n) => self.inner.handled.lock().unwrap().push(n),
                    Orderly::Stop => ctx.self_handle().stop(StopReason::Graceful).await.unwrap(),
                }
                Ok(())
            }
        }
        let actor = Front {
            inner: Orderer {
                handled: handled.clone(),
            },
            entered: entered.clone(),
            release: release.clone(),
        }
        .spawn()
        .on_system(&sys)
        .await?;
        actor.notify(Orderly::Work(0)).await?;
        entered.notified().await;
        for n in 1..=2 {
            actor.notify(Orderly::Work(n)).await?;
        }
        actor.notify(Orderly::Stop).await?;
        actor.notify(Orderly::Work(3)).await?;
        release.notify_one();
        assert_eq!(actor.wait_stopped().await, StopReason::Graceful);
        assert_eq!(*handled.lock().unwrap(), vec![0, 1, 2]);
        Ok(())
    });
}
