use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use tokio::runtime::Builder;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorSystem, Down, GroupChange, PgScope,
    RestartType, StopReason, SupervisionConfig,
};

#[derive(Default)]
struct CountingActor {
    value: u64,
}

#[derive(Clone)]
enum Msg {
    Notify(u64),
}

enum Resp {
    Ack,
}

impl Actor for CountingActor {
    type Message = Msg;
    type Response = Resp;

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            Msg::Notify(delta) => {
                self.value += delta;
                Ok(Resp::Ack)
            }
        }
    }
}

async fn spawn_many(total: usize) {
    let mut handles = Vec::with_capacity(total);
    for idx in 0..total {
        let handle = CountingActor::default()
            .spawn()
            .named(format!("bench-{idx}"))
            .with_config(ActorConfig::default())
            .await
            .expect("spawn");
        handles.push(handle);
    }

    for handle in &handles {
        handle.notify(Msg::Notify(1)).await.expect("notify");
    }

    for handle in handles {
        handle.stop(StopReason::Graceful).await.expect("stop");
    }
}

async fn ping_pong(iterations: usize) {
    let actor = CountingActor::default()
        .spawn()
        .named("ping-pong")
        .with_config(ActorConfig::default())
        .await
        .expect("spawn");

    for _ in 0..iterations {
        actor.notify(Msg::Notify(1)).await.expect("notify");
    }

    actor.stop(StopReason::Graceful).await.expect("stop");
}

/// A child that fails on every message, so each message costs one restart.
struct Crasher;

#[derive(Clone)]
enum CrashMsg {
    Crash,
}

impl Actor for Crasher {
    type Message = CrashMsg;
    type Response = ();

    async fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        Err(ActorError::user("bench crash"))
    }
}

/// A supervisor that starts one permanent `Crasher` and counts the
/// incarnations built, the first one included.
struct RestartSupervisor {
    built: Arc<AtomicUsize>,
}

impl Actor for RestartSupervisor {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let built = self.built.clone();
        ctx.spawn_child(move || {
            built.fetch_add(1, Ordering::SeqCst);
            Crasher
        })
        .named("bench-restart-child")
        .restart_type(RestartType::Permanent)
        .await?;
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        Ok(())
    }
}

/// Crashes the child `restarts` times, each time waiting for the next
/// incarnation to be alive: every crash goes through the whole restart path.
async fn supervised_restart_cycle(restarts: usize) {
    let built = Arc::new(AtomicUsize::new(0));
    let supervisor = RestartSupervisor {
        built: built.clone(),
    }
    .spawn()
    .named("bench-restart-supervisor")
    .supervisor()
    .with_supervision(SupervisionConfig::default().max_restarts(u32::MAX, Duration::from_secs(1)))
    .await
    .expect("spawn");
    let system = ActorSystem::default();

    for incarnation in 1..=restarts + 1 {
        let child = loop {
            if built.load(Ordering::SeqCst) >= incarnation {
                if let Some(child) = system.get::<Crasher>("bench-restart-child") {
                    if child.is_alive() {
                        break child;
                    }
                }
            }
            tokio::task::yield_now().await;
        };
        if incarnation <= restarts {
            child.notify(CrashMsg::Crash).await.expect("notify");
        }
    }

    supervisor.stop(StopReason::Graceful).await.expect("stop");
    supervisor.wait_stopped().await;
}

/// Counts the notices of its monitors and wakes the bench when all arrived.
struct MonitorWatcher {
    seen: usize,
    expected: usize,
    done: Arc<tokio::sync::Notify>,
}

enum WatchMsg {
    Watch(ActorHandle<CountingActor>, usize),
    Gone(Down),
}

impl Actor for MonitorWatcher {
    type Message = WatchMsg;
    type Response = ();

    async fn handle(&mut self, msg: WatchMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            WatchMsg::Watch(target, count) => {
                self.expected = count;
                for _ in 0..count {
                    ctx.monitor(&target, WatchMsg::Gone).expect("running");
                }
            }
            WatchMsg::Gone(down) => {
                std::hint::black_box(&down);
                self.seen += 1;
                if self.seen == self.expected {
                    self.done.notify_one();
                }
            }
        }
        Ok(())
    }
}

/// Ten thousand monitors on one target: the target ends and every notice is
/// handled.
async fn monitor_on_one_target(count: usize) {
    let target = CountingActor::default().spawn().await.expect("spawn");
    let done = Arc::new(tokio::sync::Notify::new());
    let watcher = MonitorWatcher {
        seen: 0,
        expected: usize::MAX,
        done: done.clone(),
    }
    .spawn()
    .await
    .expect("spawn");
    watcher
        .send(WatchMsg::Watch(target.clone(), count))
        .await
        .expect("watch");
    target.stop(StopReason::Kill).await.expect("stop");
    done.notified().await;
}

/// Follows a process group through the changes it is told of, slowly, and
/// wakes the bench when it has seen the group empty again.
struct GroupWatcher {
    members: usize,
    done: Arc<tokio::sync::Notify>,
}

enum GroupMsg {
    Watch(PgScope),
    Change(GroupChange),
}

impl Actor for GroupWatcher {
    type Message = GroupMsg;
    type Response = ();

    async fn handle(&mut self, msg: GroupMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            GroupMsg::Watch(scope) => {
                ctx.monitor_group(&scope, "bench-group", GroupMsg::Change)
                    .expect("running");
            }
            GroupMsg::Change(change) => {
                // Slower than the joins, so the subscription overflows.
                tokio::task::yield_now().await;
                match change {
                    GroupChange::Join { members, .. } => self.members += members.len(),
                    GroupChange::Leave { members, .. } => {
                        self.members = self.members.saturating_sub(members.len());
                    }
                    GroupChange::Resync { groups, .. } => self.members = groups[0].1.len(),
                }
                if self.members == 0 {
                    self.done.notify_one();
                }
            }
        }
        Ok(())
    }
}

/// Ten thousand members join one group and leave it again while one slow
/// subscriber watches; the subscriber ends up knowing the group is empty.
async fn pg_join_leave(count: usize) {
    let system = ActorSystem::create(format!("bench-pg-{}", next_system_number()))
        .expect("a fresh system name");
    let scope = system.pg();
    let done = Arc::new(tokio::sync::Notify::new());
    let watcher = GroupWatcher {
        members: 0,
        done: done.clone(),
    }
    .spawn()
    .on_system(&system)
    .await
    .expect("spawn");
    watcher
        .send(GroupMsg::Watch(scope.clone()))
        .await
        .expect("watch");
    let mut members = Vec::with_capacity(count);
    for _ in 0..count {
        members.push(
            CountingActor::default()
                .spawn()
                .on_system(&system)
                .await
                .expect("spawn"),
        );
    }
    for member in &members {
        scope.join("bench-group", member).expect("join");
    }
    for member in &members {
        scope.leave("bench-group", member).expect("leave");
    }
    done.notified().await;
}

fn next_system_number() -> u64 {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed) as u64
}

fn criterion_benchmarks(c: &mut Criterion) {
    c.bench_function("spawn_actors_10k", |b| {
        b.iter(|| {
            let rt = Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async {
                spawn_many(10_000).await;
            });
        })
    });

    c.bench_function("ping_pong_1m", |b| {
        b.iter(|| {
            let rt = Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async {
                ping_pong(1_000_000).await;
            });
        })
    });

    c.bench_function("supervised_restart_cycle", |b| {
        b.iter(|| {
            let rt = Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(120), supervised_restart_cycle(1_000))
                    .await
                    .expect("the restart cycle finishes");
            });
        })
    });

    c.bench_function("monitor_10k_on_one_target", |b| {
        b.iter(|| {
            let rt = Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(120), monitor_on_one_target(10_000))
                    .await
                    .expect("every notice is handled");
            });
        })
    });

    c.bench_function("pg_join_leave_10k", |b| {
        b.iter(|| {
            let rt = Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(120), pg_join_leave(10_000))
                    .await
                    .expect("the subscriber sees the group empty");
            });
        })
    });

    c.bench_function("ping_pong_1m_watchdog_on", |b| {
        // Bare spawns join the process-wide default system, so the threshold is
        // set there and switched off again before any other benchmark runs.
        ActorSystem::default().set_turn_warning_threshold(Some(Duration::from_secs(60)));
        b.iter(|| {
            let rt = Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async {
                ping_pong(1_000_000).await;
            });
        });
        ActorSystem::default().set_turn_warning_threshold(None);
    });
}

criterion_group!(benches, criterion_benchmarks);
criterion_main!(benches);
