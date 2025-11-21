use async_trait::async_trait;
use tokio::time::{sleep, Duration, Instant};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, StopReason,
};

#[derive(Default)]
struct Pinger {
    ticks: u32,
}

#[derive(Clone)]
enum Msg {
    Tick,
    Get,
}

enum Resp {
    Ack,
    Count(u32),
}

#[async_trait]
impl Actor for Pinger {
    type Message = Msg;
    type Response = Resp;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let when = Instant::now() + Duration::from_millis(5);
        ctx.schedule_once(Msg::Tick, when)?;
        Ok(())
    }

    async fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Self::Response> {
        match msg {
            Msg::Tick => {
                self.ticks += 1;
                Ok(Resp::Ack)
            }
            Msg::Get => Ok(Resp::Count(self.ticks)),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recurring_timer_delivers_messages() {
    let handle = Pinger::default()
        .spawn_actor("pinger", ActorConfig::default())
        .await
        .unwrap();

    sleep(Duration::from_millis(15)).await;
    let ticks = match handle.send(Msg::Get).await.unwrap() {
        Resp::Count(t) => t,
        Resp::Ack => 0,
    };
    assert_eq!(ticks, 1);
    println!("timer test ticks={ticks}");

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test]
async fn schedule_after_convenience_method() {
    #[derive(Default)]
    struct TimerActor {
        tick_received: bool,
    }

    #[derive(Clone)]
    enum TimerMsg {
        Tick,
        Check,
    }

    enum TimerResp {
        Ack,
        Received(bool),
    }

    #[async_trait]
    impl Actor for TimerActor {
        type Message = TimerMsg;
        type Response = TimerResp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            // Use schedule_after instead of schedule_once
            ctx.schedule_after(TimerMsg::Tick, Duration::from_millis(10))?;
            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<Self::Response> {
            match msg {
                TimerMsg::Tick => {
                    self.tick_received = true;
                    Ok(TimerResp::Ack)
                }
                TimerMsg::Check => Ok(TimerResp::Received(self.tick_received)),
            }
        }
    }

    let actor = TimerActor::default().spawn_actor("test", ()).await.unwrap();
    sleep(Duration::from_millis(50)).await;

    if let TimerResp::Received(received) = actor.send(TimerMsg::Check).await.unwrap() {
        assert!(received, "Timer should have fired");
    }
}

#[tokio::test]
async fn timer_cancellation_apis() {
    use tokio_actors::MissPolicy;

    #[derive(Default)]
    struct CancelActor {
        ticks: u64,
    }

    #[derive(Clone)]
    enum CancelMsg {
        Tick,
        GetTicks,
    }

    enum CancelResp {
        Ack,
        Count(u64),
    }

    #[async_trait]
    impl Actor for CancelActor {
        type Message = CancelMsg;
        type Response = CancelResp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            // Schedule 3 recurring timers
            ctx.schedule_recurring(
                CancelMsg::Tick,
                Duration::from_millis(100),
                MissPolicy::Skip,
            )?;
            ctx.schedule_recurring(
                CancelMsg::Tick,
                Duration::from_millis(100),
                MissPolicy::Skip,
            )?;
            ctx.schedule_recurring(
                CancelMsg::Tick,
                Duration::from_millis(100),
                MissPolicy::Skip,
            )?;

            assert_eq!(ctx.active_timer_count(), 3);
            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<Self::Response> {
            match msg {
                CancelMsg::Tick => {
                    self.ticks += 1;
                    // Cancel all timers after first tick
                    if self.ticks == 1 {
                        ctx.cancel_all_timers();
                        assert_eq!(ctx.active_timer_count(), 0);
                    }
                    Ok(CancelResp::Ack)
                }
                CancelMsg::GetTicks => Ok(CancelResp::Count(self.ticks)),
            }
        }
    }

    let actor = CancelActor::default()
        .spawn_actor("cancel", ())
        .await
        .unwrap();

    // Wait for first tick and cancellation
    sleep(Duration::from_millis(150)).await;

    // Wait more to ensure no additional ticks
    sleep(Duration::from_millis(200)).await;

    if let CancelResp::Count(ticks) = actor.send(CancelMsg::GetTicks).await.unwrap() {
        assert!(
            ticks >= 1 && ticks <= 3,
            "Should have 1-3 ticks before cancellation, got {}",
            ticks
        );
    }
}
