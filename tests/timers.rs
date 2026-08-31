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

// ---------------------------------------------------------------------------
// Pathological deadline duration
// ---------------------------------------------------------------------------

// A plain `Instant::now() + Duration::MAX` panics on overflow; scheduling a
// one-shot with such a delay must saturate the deadline instead of panicking,
// and the resulting far-future deadline must simply never fire within any
// realistic test window. Uses its own minimal actor (not `Pinger`, whose
// `on_started` schedules its own unrelated 10ms tick).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_with_huge_delay_does_not_panic_and_never_fires() {
    #[derive(Default)]
    struct Counter {
        ticks: u32,
    }

    #[derive(Clone)]
    enum CounterMsg {
        /// Registers the one-shot from inside a real callback: `schedule` is
        /// only available on `ActorContext`, never on an external handle.
        ScheduleHugeDelay,
        Tick,
        Get,
    }

    impl Actor for Counter {
        type Message = CounterMsg;
        type Response = u32;

        async fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<u32> {
            match msg {
                CounterMsg::ScheduleHugeDelay => {
                    ctx.schedule(CounterMsg::Tick).after(Duration::MAX).await?;
                    Ok(self.ticks)
                }
                CounterMsg::Tick => {
                    self.ticks += 1;
                    Ok(self.ticks)
                }
                CounterMsg::Get => Ok(self.ticks),
            }
        }
    }

    let handle = Counter::default()
        .spawn()
        .named("huge-delay")
        .await
        .unwrap();

    handle
        .send(CounterMsg::ScheduleHugeDelay)
        .await
        .expect("scheduling with a huge delay must not panic");

    sleep(Duration::from_millis(200)).await;
    let ticks = handle.send(CounterMsg::Get).await.unwrap();
    assert_eq!(ticks, 0, "a saturated far-future deadline must not fire");

    handle.stop(StopReason::Graceful).await.unwrap();
}

// ---------------------------------------------------------------------------
// Forwarder plane: reaping and panic surfacing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fired_one_shot_leaves_zero_registrations() {
    struct OneShotActor;

    #[derive(Clone)]
    enum Msg {
        Tick,
        GetCount,
    }

    impl Actor for OneShotActor {
        type Message = Msg;
        type Response = usize;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            ctx.schedule(Msg::Tick)
                .after(Duration::from_millis(10))
                .await?;
            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<usize> {
            match msg {
                Msg::Tick | Msg::GetCount => Ok(ctx.active_timer_count()),
            }
        }
    }

    let handle = OneShotActor.spawn().named("one-shot-reap").await.unwrap();

    // Poll instead of assuming a wall-clock window: CI runners stall
    // arbitrarily long. Once the one-shot fires it must be reaped from the
    // registration count - never merely cancelled/removed by an explicit
    // call, since nothing here ever calls `cancel_timer`.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let count = handle.send(Msg::GetCount).await.unwrap();
        if count == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a fired one-shot must be reaped from active_timer_count within 5s"
        );
        sleep(Duration::from_millis(10)).await;
    }

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_recurring_timer_reaps_its_registration() {
    #[derive(Default)]
    struct CancelReapActor {
        id: Option<tokio_actors::RecurringId>,
    }

    #[derive(Clone)]
    enum Msg {
        Tick,
        CancelIt,
        GetCount,
    }

    impl Actor for CancelReapActor {
        type Message = Msg;
        type Response = usize;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            let id = ctx
                .schedule(Msg::Tick)
                .every(Duration::from_millis(10))
                .await?;
            self.id = Some(id);
            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<usize> {
            match msg {
                Msg::Tick => Ok(ctx.active_timer_count()),
                Msg::CancelIt => {
                    ctx.cancel_timer(self.id.take().unwrap()).unwrap();
                    Ok(ctx.active_timer_count())
                }
                Msg::GetCount => Ok(ctx.active_timer_count()),
            }
        }
    }

    let handle = CancelReapActor::default()
        .spawn()
        .named("cancel-reap")
        .await
        .unwrap();

    // Wait for at least one real tick before cancelling, so the cancel is
    // exercised against a timer actually in flight, not one that never had
    // the chance to fire.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if handle.send(Msg::GetCount).await.unwrap() == 1 {
            sleep(Duration::from_millis(15)).await;
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the recurring timer must be registered within 5s"
        );
        sleep(Duration::from_millis(5)).await;
    }

    let after_cancel = handle.send(Msg::CancelIt).await.unwrap();
    assert_eq!(
        after_cancel, 0,
        "cancelling the only recurring timer must reap its registration immediately"
    );

    // Holds after the cancel too: the background loop noticing its
    // cancellation token and exiting must not resurrect the registration.
    sleep(Duration::from_millis(100)).await;
    assert_eq!(handle.send(Msg::GetCount).await.unwrap(), 0);

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_recurring_factory_surfaces_without_stopping_the_actor() {
    struct PanicActor;

    enum Msg {
        CheckError,
    }

    impl Actor for PanicActor {
        type Message = Msg;
        type Response = bool;

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            ctx.every_with(Duration::from_millis(10), || {
                panic!("forwarder factory boom");
            })
            .await?;
            Ok(())
        }

        async fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<bool> {
            match msg {
                Msg::CheckError => Ok(ctx.last_forwarder_error().is_some()),
            }
        }
    }

    let handle = PanicActor
        .spawn()
        .named("panicking-forwarder")
        .await
        .unwrap();

    // Every poll is a real round trip through `handle`: reaching a `true`
    // response proves both that the panic surfaced in
    // `last_forwarder_error` and that the actor kept handling messages the
    // whole time - a forwarder's death is never the actor's death.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if handle.send(Msg::CheckError).await.unwrap() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a panicking forwarder factory must surface in last_forwarder_error within 5s"
        );
        sleep(Duration::from_millis(10)).await;
    }

    // The actor is still fully responsive after the forwarder's panic.
    assert!(handle.send(Msg::CheckError).await.unwrap());

    handle.stop(StopReason::Graceful).await.unwrap();
}
