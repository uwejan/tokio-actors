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

impl Actor for Pinger {
    type Message = Msg;
    type Response = Resp;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.schedule(Msg::Tick)
            .after(Duration::from_millis(10))
            .await?;
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
        .spawn()
        .named("pinger")
        .with_config(ActorConfig::default())
        .await
        .unwrap();

    // Poll for the tick instead of assuming a wall-clock window: CI runners
    // stall arbitrarily long. The one-shot semantic (exactly one fire) is
    // then checked with a negative quiet window.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ticks = 0;
    while ticks < 1 {
        assert!(
            Instant::now() < deadline,
            "one-shot schedule must fire within 5s"
        );
        sleep(Duration::from_millis(10)).await;
        ticks = match handle.send(Msg::Get).await.unwrap() {
            Resp::Count(t) => t,
            Resp::Ack => 0,
        };
    }
    sleep(Duration::from_millis(100)).await;
    let after = match handle.send(Msg::Get).await.unwrap() {
        Resp::Count(t) => t,
        Resp::Ack => 0,
    };
    assert_eq!(after, 1, "a one-shot schedule must fire exactly once");

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

    impl Actor for TimerActor {
        type Message = TimerMsg;
        type Response = TimerResp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            ctx.schedule(TimerMsg::Tick)
                .after(Duration::from_millis(10))
                .await?;
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

    let actor = TimerActor::default()
        .spawn()
        .named("timer-recurring")
        .await
        .unwrap();

    // Poll instead of a fixed window: CI runners stall arbitrarily long.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let TimerResp::Received(true) = actor.send(TimerMsg::Check).await.unwrap() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the scheduled one-shot must fire within 5s"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn timer_cancellation_apis() {
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

    impl Actor for CancelActor {
        type Message = CancelMsg;
        type Response = CancelResp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            // Schedule 3 recurring timers
            ctx.schedule(CancelMsg::Tick)
                .every(Duration::from_millis(100))
                .await?;
            ctx.schedule(CancelMsg::Tick)
                .every(Duration::from_millis(100))
                .await?;
            ctx.schedule(CancelMsg::Tick)
                .every(Duration::from_millis(100))
                .await?;

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
        .spawn()
        .named("cancel")
        .await
        .unwrap();

    // Poll for the first tick (which triggers cancel_all_timers) instead of
    // assuming a wall-clock window, then hold a quiet window to prove the
    // cancellation actually silenced the other recurring timers.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ticks = match actor.send(CancelMsg::GetTicks).await.unwrap() {
            CancelResp::Count(t) => t,
            CancelResp::Ack => 0,
        };
        if ticks >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the recurring timers must deliver a first tick within 5s"
        );
        sleep(Duration::from_millis(10)).await;
    }
    sleep(Duration::from_millis(300)).await;
    if let CancelResp::Count(ticks) = actor.send(CancelMsg::GetTicks).await.unwrap() {
        assert!(
            (1..=3).contains(&ticks),
            "cancel_all_timers must silence the timers (at most the ticks \
             already in flight when cancel ran), got {}",
            ticks
        );
    }
}

#[tokio::test]
async fn dropped_one_shot_never_fires_only_awaited_one_registers() {
    #[derive(Default)]
    struct LazyActor {
        ticks: u32,
    }

    #[derive(Clone)]
    enum LazyMsg {
        DropUnawaited,
        ScheduleAwaited,
        Tick,
        GetTicks,
    }

    enum LazyResp {
        Ack,
        Count(u32),
    }

    impl Actor for LazyActor {
        type Message = LazyMsg;
        type Response = LazyResp;

        async fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<Self::Response> {
            match msg {
                LazyMsg::DropUnawaited => {
                    // Built and immediately discarded: registration lives
                    // entirely inside `.await`, so a builder that is never
                    // awaited arms nothing. `let _ =` silences the
                    // `#[must_use]` warning on purpose, the same way a
                    // migrating caller would if this were intentional.
                    let _ = ctx.schedule(LazyMsg::Tick).after(Duration::from_millis(10));
                    Ok(LazyResp::Ack)
                }
                LazyMsg::ScheduleAwaited => {
                    ctx.schedule(LazyMsg::Tick)
                        .after(Duration::from_millis(10))
                        .await?;
                    Ok(LazyResp::Ack)
                }
                LazyMsg::Tick => {
                    self.ticks += 1;
                    Ok(LazyResp::Ack)
                }
                LazyMsg::GetTicks => Ok(LazyResp::Count(self.ticks)),
            }
        }
    }

    let actor = LazyActor::default()
        .spawn()
        .named("lazy-one-shot")
        .await
        .unwrap();

    // Construct-and-drop: no `.await` ever touches the builder.
    actor.send(LazyMsg::DropUnawaited).await.unwrap();
    sleep(Duration::from_millis(50)).await;
    if let LazyResp::Count(ticks) = actor.send(LazyMsg::GetTicks).await.unwrap() {
        assert_eq!(ticks, 0, "a dropped, un-awaited one-shot must never fire");
    }

    // Same delay, this time awaited: it must actually register and fire.
    // Poll for the tick (CI runners stall arbitrarily long), then hold a
    // quiet window to prove it fired exactly once.
    actor.send(LazyMsg::ScheduleAwaited).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ticks = match actor.send(LazyMsg::GetTicks).await.unwrap() {
            LazyResp::Count(t) => t,
            LazyResp::Ack => 0,
        };
        if ticks >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "an awaited one-shot must fire within 5s"
        );
        sleep(Duration::from_millis(10)).await;
    }
    sleep(Duration::from_millis(100)).await;
    if let LazyResp::Count(ticks) = actor.send(LazyMsg::GetTicks).await.unwrap() {
        assert_eq!(ticks, 1, "an awaited one-shot must fire exactly once");
    }

    actor.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test]
async fn every_with_delivers_non_clone_messages() {
    // Deliberately NOT `Clone` - `every_with` must not require it, unlike
    // `every()`.
    enum EveryWithMsg {
        Tick(u32),
        GetTicks,
    }

    enum EveryWithResp {
        Ack,
        Count(u32),
    }

    #[derive(Default)]
    struct EveryWithActor {
        ticks: u32,
        last_seen: u32,
    }

    impl Actor for EveryWithActor {
        type Message = EveryWithMsg;
        type Response = EveryWithResp;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            // A stateful FnMut factory: only possible because the factory is
            // Box-owned by a single forwarder task rather than shared via Arc.
            let mut generated = 0u32;
            ctx.every_with(Duration::from_millis(20), move || {
                generated += 1;
                EveryWithMsg::Tick(generated)
            })
            .await?;
            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<Self::Response> {
            match msg {
                EveryWithMsg::Tick(n) => {
                    assert!(n > self.last_seen, "factory state must persist per tick");
                    self.last_seen = n;
                    self.ticks += 1;
                    Ok(EveryWithResp::Ack)
                }
                EveryWithMsg::GetTicks => Ok(EveryWithResp::Count(self.ticks)),
            }
        }
    }

    let actor = EveryWithActor::default()
        .spawn()
        .named("every-with")
        .await
        .unwrap();

    // Poll instead of a fixed window: CI runners stall arbitrarily long, and
    // the semantics under test (repeated delivery, per-tick factory state)
    // are proven by the counter reaching 2 whenever that happens. The
    // monotonic `last_seen` assertion in `handle` keeps guarding the factory
    // state on every tick.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ticks = match actor.send(EveryWithMsg::GetTicks).await.unwrap() {
            EveryWithResp::Count(t) => t,
            EveryWithResp::Ack => 0,
        };
        if ticks >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "every_with must deliver at least 2 ticks within 5s, got {ticks}"
        );
        sleep(Duration::from_millis(10)).await;
    }

    actor.stop(StopReason::Graceful).await.unwrap();
}
