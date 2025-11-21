use async_trait::async_trait;
use criterion::{criterion_group, criterion_main, Criterion};
use tokio::runtime::Builder;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason,
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

#[async_trait]
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
        let actor = CountingActor::default();
        let handle = actor
            .spawn_actor(format!("bench-{idx}"), ActorConfig::default())
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
        .spawn_actor("ping-pong", ActorConfig::default())
        .await
        .expect("spawn");

    for _ in 0..iterations {
        actor.notify(Msg::Notify(1)).await.expect("notify");
    }

    let _ = actor.stop(StopReason::Graceful).await.expect("stop");
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
}

criterion_group!(benches, criterion_benchmarks);
criterion_main!(benches);
