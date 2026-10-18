use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use tokio::runtime::Builder;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorResult, ActorSystem, RestartType, StopReason, SupervisionConfig,
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
