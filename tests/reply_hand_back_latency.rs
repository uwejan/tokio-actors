//! A requester woken by a reply is not left behind a whole backlog, for an
//! actor with a priority lane, a supervisor with a child, and an actor with a
//! timer, as it is not for a plain actor.

use std::time::Duration;

use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, ActorSystem,
};

mod common;

struct Kid;
impl Actor for Kid {
    type Message = ();
    type Response = ();
    async fn handle(&mut self, _m: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[derive(Clone)]
enum M {
    Work,
    Kid,
    Timer,
}

struct Busy;
impl Actor for Busy {
    type Message = M;
    type Response = u8;
    async fn handle(&mut self, m: M, ctx: &mut ActorContext<Self>) -> ActorResult<u8> {
        match m {
            M::Work => {
                let mut x = 0u64;
                for i in 0..400_000u64 {
                    x = std::hint::black_box(x.wrapping_add(i));
                }
                std::hint::black_box(x);
            }
            M::Kid => {
                let _ = ctx.spawn_child(|| Kid).await;
            }
            M::Timer => {
                let _ = ctx.schedule(M::Work).every(Duration::from_secs(3600)).await;
            }
        }
        Ok(0)
    }
}

async fn measure(label: &str, cfg: ActorConfig, setup: Vec<M>) -> Duration {
    let sys = ActorSystem::create(format!(
        "hand-back-{label}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
    .unwrap();
    let h = Busy
        .spawn()
        .named("busy")
        .on_system(&sys)
        .with_config(cfg)
        .await
        .unwrap();
    for m in setup {
        h.send(m).await.unwrap();
    }
    let mut prods = Vec::new();
    for _ in 0..4 {
        let h = h.clone();
        prods.push(tokio::spawn(async move {
            for _ in 0..4_000 {
                let _ = h.notify(M::Work).await;
            }
        }));
    }
    sleep(Duration::from_millis(100)).await;
    let mut lat = Vec::new();
    while !prods.iter().all(|p| p.is_finished()) && lat.len() < 60 {
        let t0 = Instant::now();
        let r = timeout(Duration::from_secs(5), h.get_status()).await;
        assert!(matches!(r, Ok(Ok(_))));
        lat.push(t0.elapsed());
        sleep(Duration::from_millis(5)).await;
    }
    lat.sort();
    let med = lat[lat.len() / 2];
    eprintln!(
        "{label}: {} calls, median {:?}, worst {:?}",
        lat.len(),
        med,
        lat[lat.len() - 1]
    );
    h.stop(tokio_actors::StopReason::Kill).await.ok();
    for p in prods {
        let _ = p.await;
    }
    med
}

#[test]
fn reply_wake_latency_by_actor_shape() {
    common::run_current_thread_only(async {
        let plain = measure("plain", ActorConfig::default(), vec![]).await;
        let prio = measure(
            "priority-lane",
            ActorConfig::default().with_priority_lane(64),
            vec![],
        )
        .await;
        let sup = measure(
            "supervisor-with-a-child",
            ActorConfig::default().supervisor(),
            vec![M::Kid],
        )
        .await;
        let timer = measure(
            "supervisor-with-a-timer",
            ActorConfig::default().supervisor(),
            vec![M::Timer],
        )
        .await;
        let plain_timer =
            measure("plain-with-a-timer", ActorConfig::default(), vec![M::Timer]).await;
        eprintln!("plain {plain:?} prio {prio:?} sup {sup:?} suptimer {timer:?} plaintimer {plain_timer:?}");
        // A reply-woken requester runs after the allowance of turns (a few
        // handler runs), not after the whole cooperative window of the actor.
        let bound = Duration::from_millis(30);
        assert!(
            sup < bound,
            "supervisor with a child: median {sup:?} (plain {plain:?})"
        );
        assert!(
            timer < bound,
            "supervisor with a timer: median {timer:?} (plain {plain:?})"
        );
    });
}
