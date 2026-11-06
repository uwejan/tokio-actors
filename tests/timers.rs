use tokio::time::Duration;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, MissPolicy, StopReason, TimerError,
};

mod common;

// ---------------------------------------------------------------------------
// Pathological deadline duration
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Forwarder plane: reaping and panic surfacing
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Zero-length recurring interval
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_interval_is_rejected_at_registration() {
    struct ZeroIntervalActor;

    #[derive(Clone)]
    enum ZeroMsg {
        Tick,
        TryEvery(MissPolicy),
        TryEveryWith(MissPolicy),
        Ping,
    }

    enum ZeroResp {
        Registered(Result<tokio_actors::RecurringId, TimerError>),
        Pong,
    }

    impl Actor for ZeroIntervalActor {
        type Message = ZeroMsg;
        type Response = ZeroResp;

        async fn handle(
            &mut self,
            msg: Self::Message,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<Self::Response> {
            match msg {
                ZeroMsg::TryEvery(policy) => {
                    let res = ctx
                        .schedule(ZeroMsg::Tick)
                        .every(Duration::ZERO)
                        .on_miss(policy)
                        .await;
                    Ok(ZeroResp::Registered(res))
                }
                ZeroMsg::TryEveryWith(policy) => {
                    let res = ctx
                        .every_with(Duration::ZERO, || ZeroMsg::Tick)
                        .on_miss(policy)
                        .await;
                    Ok(ZeroResp::Registered(res))
                }
                ZeroMsg::Tick => Ok(ZeroResp::Pong),
                ZeroMsg::Ping => Ok(ZeroResp::Pong),
            }
        }
    }

    let handle = ZeroIntervalActor
        .spawn()
        .named("zero-interval")
        .await
        .unwrap();

    for policy in [MissPolicy::Skip, MissPolicy::Delay, MissPolicy::CatchUp] {
        let ZeroResp::Registered(res) = handle.send(ZeroMsg::TryEvery(policy)).await.unwrap()
        else {
            panic!("TryEvery must reply with Registered");
        };
        assert!(
            matches!(res, Err(TimerError::ZeroInterval)),
            "every(Duration::ZERO) with {policy:?} must return Err(TimerError::ZeroInterval)"
        );

        let ZeroResp::Registered(res) = handle.send(ZeroMsg::TryEveryWith(policy)).await.unwrap()
        else {
            panic!("TryEveryWith must reply with Registered");
        };
        assert!(
            matches!(res, Err(TimerError::ZeroInterval)),
            "every_with(Duration::ZERO) with {policy:?} must return Err(TimerError::ZeroInterval)"
        );
    }

    // The actor must still be fully responsive: registration rejected the
    // timer before anything that could wedge the runtime was ever spawned.
    assert!(matches!(
        handle.send(ZeroMsg::Ping).await.unwrap(),
        ZeroResp::Pong
    ));

    handle.stop(StopReason::Graceful).await.unwrap();
}

// ---------------------------------------------------------------------------
// CatchUp under a full mailbox
// ---------------------------------------------------------------------------

mod catch_up_pacing {
    use super::common;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::watch;
    use tokio::time::{advance, Duration, Instant};
    use tokio_actors::{
        actor::{context::ActorContext, Actor, ActorExt},
        ActorConfig, ActorHandle, ActorResult, MissPolicy,
    };

    const INTERVAL: Duration = Duration::from_millis(10);
    const INTERVALS: usize = 10;

    /// A tick: counted as a loss when it is dropped without being handled,
    /// which is what a full mailbox does to the tick a burst has no room for.
    struct Tick {
        handled: bool,
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for Tick {
        fn drop(&mut self) {
            if !self.handled {
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    struct Held {
        calls: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
        delivered: Arc<AtomicUsize>,
        gate: watch::Receiver<bool>,
    }

    impl Actor for Held {
        type Message = Tick;
        type Response = ();

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            let calls = self.calls.clone();
            let dropped = self.dropped.clone();
            ctx.every_with(INTERVAL, move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Tick {
                    handled: false,
                    dropped: dropped.clone(),
                }
            })
            .on_miss(MissPolicy::CatchUp)
            .await?;
            Ok(())
        }

        async fn handle(
            &mut self,
            mut tick: Tick,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<()> {
            self.gate
                .wait_for(|open| *open)
                .await
                .expect("the gate outlives the actor");
            tick.handled = true;
            self.delivered.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Lets every runnable task run. The clock is paused, so nothing here
    /// moves it.
    async fn settle() {
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
    }

    /// Yields until `done` holds, and fails after a bound that no ordering of
    /// the tasks needs.
    async fn until(what: &str, done: impl Fn() -> bool) {
        for _ in 0..20_000 {
            if done() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("{what}");
    }

    /// A `CatchUp` timer whose mailbox is full loses no tick and is paced by
    /// the consumer: ten intervals pass at once while the actor is held, the
    /// timer delivers what the mailbox has room for and waits, and once the
    /// actor is released every one of the ten deadlines is handled, each
    /// exactly once. Each time a burst finds the mailbox full, the factory
    /// has made one tick that is dropped and made again, so the factory runs
    /// once more for every such pause than ticks are handled.
    #[test]
    fn a_catch_up_timer_under_a_full_mailbox_loses_no_tick() {
        common::run_paused(async {
            let calls = Arc::new(AtomicUsize::new(0));
            let dropped = Arc::new(AtomicUsize::new(0));
            let delivered = Arc::new(AtomicUsize::new(0));
            let (gate_tx, gate_rx) = watch::channel(false);
            let started = Instant::now();
            let handle: ActorHandle<Held> = Held {
                calls: calls.clone(),
                dropped: dropped.clone(),
                delivered: delivered.clone(),
                gate: gate_rx,
            }
            .spawn()
            .with_config(ActorConfig::default().with_mailbox_capacity(2))
            .await
            .unwrap();

            advance(INTERVAL * INTERVALS as u32).await;
            settle().await;
            assert_eq!(
                started.elapsed(),
                INTERVAL * INTERVALS as u32,
                "only the test moved the clock"
            );

            // The actor holds one tick in its handler and the mailbox holds
            // two: the timer has delivered no more than the consumer takes.
            assert_eq!(delivered.load(Ordering::SeqCst), 0);
            assert_eq!(handle.mailbox_len(), 2, "the mailbox is full");
            let paused = dropped.load(Ordering::SeqCst);
            assert!(paused >= 1, "the burst met the full mailbox");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                3 + paused + 1,
                "three ticks delivered, one made again for each pause, one waiting for room"
            );

            gate_tx.send(true).unwrap();
            until("every elapsed deadline is handled", || {
                delivered.load(Ordering::SeqCst) == INTERVALS
            })
            .await;
            settle().await;

            assert_eq!(
                delivered.load(Ordering::SeqCst),
                INTERVALS,
                "one tick per elapsed deadline, none lost and none twice"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                INTERVALS + dropped.load(Ordering::SeqCst),
                "the factory runs once more for each pause"
            );
            assert!(dropped.load(Ordering::SeqCst) >= paused);
        });
    }
}
