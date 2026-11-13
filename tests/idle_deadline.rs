#![allow(dead_code)]
//! The receive timeout and the idle hook: one spell per stretch of waiting,
//! which a message ends and a system request neither ends nor restarts; each
//! threshold fires once per spell, inline, below every other branch.
//!
//! Every test runs on a paused clock, so each deadline lands on its exact
//! virtual instant and the tests order by observation.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorStatus, SpawnError, StopReason,
};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A handler's gate: the sender that says it entered and the receiver that
/// releases it, taken by the handler that uses them.
type Gate = Arc<Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>>;

/// What the actor saw, each entry with the virtual time it happened at.
#[derive(Clone)]
struct Log {
    start: Instant,
    entries: Arc<Mutex<Vec<(String, Duration)>>>,
}

impl Log {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn push(&self, what: &str) {
        let at = self.start.elapsed();
        self.entries.lock().unwrap().push((what.to_string(), at));
    }

    fn labels(&self) -> Vec<String> {
        self.all().into_iter().map(|(what, _)| what).collect()
    }

    /// The entries, cut after a dozen so a runaway log fails an assertion
    /// without printing itself: a longer log ends in a marker entry.
    fn all(&self) -> Vec<(String, Duration)> {
        let entries = self.entries.lock().unwrap();
        let mut shown: Vec<_> = entries.iter().take(12).cloned().collect();
        if entries.len() > 12 {
            shown.push((format!("and {} more", entries.len() - 12), Duration::ZERO));
        }
        shown
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    MapPanics,
    IdleErr,
    IdlePanics,
    TickErr,
}

enum Msg {
    Work,
    Tick,
}

#[derive(Clone)]
struct Probe {
    receive: Option<u64>,
    idle: Option<u64>,
    log: Log,
    fault: Fault,
    /// The receive handler queues a message, says it has entered and waits to
    /// be released.
    tick_gate: Gate,
    /// The receive handler asks for a `Kill` of its own actor.
    tick_kills: bool,
    /// The receive handler sets the receive timeout again.
    reset_on_tick: bool,
    /// The facts the fire sees about the actor, `(len, available, status)`.
    facts: Arc<Mutex<Vec<(usize, usize, ActorStatus)>>>,
}

impl Probe {
    fn new(receive: Option<u64>, idle: Option<u64>) -> Self {
        Self {
            receive,
            idle,
            log: Log::new(),
            fault: Fault::None,
            tick_gate: Arc::new(Mutex::new(None)),
            tick_kills: false,
            reset_on_tick: false,
            facts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn note_facts(&self, ctx: &ActorContext<Self>) {
        let me = ctx.self_handle();
        self.facts
            .lock()
            .unwrap()
            .push((me.mailbox_len(), me.mailbox_available(), ctx.status()));
    }
}

impl Actor for Probe {
    type Message = Msg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(after) = self.receive {
            if self.fault == Fault::MapPanics {
                ctx.receive_timeout(ms(after), || panic!("the map panicked"));
            } else {
                ctx.receive_timeout(ms(after), || Msg::Tick);
            }
        }
        if let Some(after) = self.idle {
            ctx.idle_after(ms(after));
        }
        Ok(())
    }

    async fn on_idle(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.log.push("idle");
        self.note_facts(ctx);
        match self.fault {
            Fault::IdleErr => Err(ActorError::user("idle failed")),
            Fault::IdlePanics => panic!("the idle hook panicked"),
            _ => Ok(()),
        }
    }

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Work => self.log.push("work"),
            Msg::Tick => {
                self.log.push("tick");
                self.note_facts(ctx);
                if self.fault == Fault::TickErr {
                    return Err(ActorError::user("tick failed"));
                }
                if self.reset_on_tick {
                    ctx.receive_timeout(ms(100), || Msg::Tick);
                }
                let me = ctx.self_handle();
                let gate = self.tick_gate.lock().unwrap().take();
                if let Some((entered, release)) = gate {
                    me.try_notify(Msg::Work).expect("room for one message");
                    let _ = entered.send(());
                    let _ = release.await;
                }
                if self.tick_kills {
                    tokio::spawn(async move {
                        let _ = me.stop(StopReason::Kill).await;
                    });
                    tokio::task::yield_now().await;
                }
            }
        }
        Ok(())
    }
}

async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

/// Moves the clock in steps of 10 ms, letting every task run between two, so a
/// fire is stamped with the instant of its deadline and not with the end of one
/// jump.
async fn advance(by: Duration) {
    let mut left = by;
    while !left.is_zero() {
        let step = left.min(ms(10));
        tokio::time::advance(step).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        left -= step;
    }
    settle().await;
}

async fn start(probe: Probe) -> ActorHandle<Probe> {
    let handle = probe
        .spawn()
        .with_config(ActorConfig::default().with_mailbox_capacity(1))
        .await
        .unwrap();
    settle().await;
    handle
}

/// A message that is queued when a threshold is due is handled first and
/// nothing fires. The receive timeout fires and its handler queues a message
/// and holds the actor; a status request from outside every runtime task goes
/// in meanwhile, and the actor answers it and reads the select for a few
/// turns with the idle threshold overdue and a message waiting. The message
/// is handled, the hook does not run.
#[test]
fn fires_only_when_idle() {
    common::run_paused(async {
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let probe = Probe::new(Some(100), Some(100));
        *probe.tick_gate.lock().unwrap() = Some((entered, release_rx));
        let log = probe.log.clone();
        let handle = start(probe).await;
        tokio::time::advance(ms(100)).await;
        entered_rx.await.unwrap();
        let status = handle.get_status();
        tokio::pin!(status);
        assert!(
            tokio::time::timeout(Duration::ZERO, &mut status)
                .await
                .is_err(),
            "the actor is held, so the request is not answered yet"
        );
        release.send(()).unwrap();
        status.await.unwrap();
        settle().await;
        assert_eq!(log.labels(), ["tick", "work"], "{:?}", log.all());
    });
}

/// A status request every third of the period, for three periods, leaves the
/// hook firing once, at the end of the first period.
#[test]
fn a_system_request_does_not_restart_the_spell() {
    common::run_paused(async {
        let probe = Probe::new(None, Some(300));
        let log = probe.log.clone();
        let handle = start(probe).await;
        for _ in 0..9 {
            advance(ms(100)).await;
            handle.get_status().await.unwrap();
        }
        assert_eq!(log.all(), [("idle".to_string(), ms(300))]);
    });
}

/// Each threshold fires once per spell, and again in the next spell, which a
/// message starts.
#[test]
fn fires_once_per_spell_and_again_after_a_message() {
    common::run_paused(async {
        let probe = Probe::new(Some(100), Some(100));
        let log = probe.log.clone();
        let handle = start(probe).await;
        advance(ms(1000)).await;
        assert_eq!(
            log.all(),
            [("tick".to_string(), ms(100)), ("idle".to_string(), ms(100))]
        );
        handle.notify(Msg::Work).await.unwrap();
        settle().await;
        advance(ms(100)).await;
        assert_eq!(
            log.all(),
            [
                ("tick".to_string(), ms(100)),
                ("idle".to_string(), ms(100)),
                ("work".to_string(), ms(1000)),
                ("tick".to_string(), ms(1100)),
                ("idle".to_string(), ms(1100)),
            ]
        );
    });
}

/// A handler that sets the receive timeout again replaces its length and
/// function without restarting the spell: the timeout does not fire a second
/// time before the next message.
#[test]
fn a_timeout_set_again_in_the_spell_does_not_fire_again() {
    common::run_paused(async {
        let mut probe = Probe::new(Some(100), None);
        probe.reset_on_tick = true;
        let log = probe.log.clone();
        let handle = start(probe).await;
        advance(ms(1000)).await;
        assert_eq!(log.all(), [("tick".to_string(), ms(100))]);
        handle.notify(Msg::Work).await.unwrap();
        settle().await;
        advance(ms(100)).await;
        assert_eq!(
            log.labels(),
            ["tick", "work", "tick"],
            "the next spell fires once more: {:?}",
            log.all()
        );
    });
}

/// A length of zero fires each threshold once and never spins.
#[test]
fn zero_fires_once_and_never_spins() {
    common::run_paused(async {
        let probe = Probe::new(Some(0), Some(0));
        let log = probe.log.clone();
        let _handle = start(probe).await;
        advance(ms(10)).await;
        assert_eq!(log.labels(), ["tick", "idle"]);
    });
}

/// A `Kill` stored when the spell's next threshold is due ends the actor, and
/// the hook never runs.
#[test]
fn kill_beats_the_fire() {
    common::run_paused(async {
        let mut probe = Probe::new(Some(100), Some(100));
        probe.tick_kills = true;
        let log = probe.log.clone();
        let handle = start(probe).await;
        advance(ms(100)).await;
        assert_eq!(handle.wait_stopped().await, StopReason::Kill);
        assert_eq!(log.labels(), ["tick"], "the hook did not run");
    });
}

/// A deadline or a timer needs the time driver: on a runtime built without it
/// the call panics, here inside the start hook, so the start fails with the
/// panic's text and nothing is registered.
#[test]
fn setting_a_deadline_without_the_time_driver_panics_like_a_timer() {
    #[derive(Clone, Copy)]
    enum Which {
        Receive,
        Idle,
        OneShot,
        Recurring,
        RecurringWith,
    }
    struct Late(Which);
    impl Actor for Late {
        type Message = ();
        type Response = ();
        async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            match self.0 {
                Which::Receive => ctx.receive_timeout(ms(10), || ()),
                Which::Idle => ctx.idle_after(ms(10)),
                Which::OneShot => {
                    let _ = ctx.schedule(()).after(ms(10)).await;
                }
                Which::Recurring => {
                    let _ = ctx.schedule(()).every(ms(10)).await;
                }
                Which::RecurringWith => {
                    let _ = ctx.every_with(ms(10), || ()).await;
                }
            }
            Ok(())
        }
        async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
    }
    for which in [
        Which::Receive,
        Which::Idle,
        Which::OneShot,
        Which::Recurring,
        Which::RecurringWith,
    ] {
        let runtime = Builder::new_current_thread().build().unwrap();
        let started = runtime.block_on(async { Late(which).spawn().await });
        match started {
            Err(SpawnError::Init(error)) => {
                let text = error.to_string();
                assert!(text.contains("needs the Tokio time driver"), "{text}");
            }
            other => panic!("expected the start to fail, got {:?}", other.map(|_| ())),
        }
    }
}

// ---------------------------------------------------------------------------
// An actor that uses every other piece of the machinery
// ---------------------------------------------------------------------------

/// The peer the crowd links to and monitors; it never ends.
struct Peer;

impl Actor for Peer {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A callee that never answers, so a request to it stays outstanding.
struct Silent;

impl Actor for Silent {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        std::future::pending::<()>().await;
        Ok(())
    }
}

enum CrowdMsg {
    Work,
    Tick,
    /// What the monitor, the link, the group monitor and the request would
    /// deliver; none arrives in the scenario.
    Other,
}

/// Holds a link, a monitor, a group monitor, an alias and an outstanding
/// request, and has both thresholds set.
struct Crowd {
    peer: ActorHandle<Peer>,
    silent: ActorHandle<Silent>,
    scope: tokio_actors::PgScope,
    log: Log,
    alias: Option<tokio_actors::Alias<Crowd>>,
}

impl Actor for Crowd {
    type Message = CrowdMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.link(&self.peer).expect("the crowd is running");
        ctx.monitor(&self.peer, |_| CrowdMsg::Other)
            .expect("the crowd is running");
        ctx.monitor_group(&self.scope, "crowd", |_| CrowdMsg::Other)
            .expect("the crowd is running");
        self.alias = Some(ctx.alias(tokio_actors::AliasOptions::default()));
        ctx.request(&self.silent, (), |_| CrowdMsg::Other)
            .expect("the crowd is running");
        ctx.receive_timeout(ms(100), || CrowdMsg::Tick);
        ctx.idle_after(ms(100));
        Ok(())
    }

    async fn on_idle(&mut self, _: &mut ActorContext<Self>) -> ActorResult<()> {
        self.log.push("idle");
        Ok(())
    }

    async fn handle(&mut self, msg: CrowdMsg, _: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            CrowdMsg::Work => self.log.push("work"),
            CrowdMsg::Tick => self.log.push("tick"),
            CrowdMsg::Other => self.log.push("other"),
        }
        Ok(())
    }
}

/// A stop asked for by a callback other than `handle`, and vetoed, leaves no
/// mark on the idle hook that fires next: the message that hook continues with
/// runs.
#[test]
fn a_vetoed_stop_from_another_callback_does_not_drop_the_idle_hooks_continuation() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio_actors::{ChildEvent, RestartType};

    enum Say {
        Spawn(oneshot::Sender<ActorHandle<Leaf>>),
        Marker,
    }
    struct Leaf;
    impl Actor for Leaf {
        type Message = ();
        type Response = ();
        async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
    }
    struct Owner {
        markers: Arc<AtomicU32>,
        vetoed: bool,
    }
    impl Actor for Owner {
        type Message = Say;
        type Response = ();
        async fn handle(&mut self, msg: Say, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            match msg {
                Say::Spawn(reply) => {
                    let child = ctx
                        .spawn_child(|| Leaf)
                        .restart_type(RestartType::Temporary)
                        .await?;
                    let _ = reply.send(child);
                }
                Say::Marker => {
                    self.markers.fetch_add(1, Ordering::SeqCst);
                }
            }
            Ok(())
        }
        async fn on_child_stopped(
            &mut self,
            _: &ChildEvent,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<()> {
            ctx.idle_after(Duration::ZERO);
            let _ = ctx.stop(StopReason::Graceful);
            Ok(())
        }
        async fn pre_stop(&mut self, _: &StopReason, _: &mut ActorContext<Self>) -> bool {
            let veto = !self.vetoed;
            self.vetoed = true;
            !veto
        }
        async fn on_idle(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            ctx.continue_with(Say::Marker);
            Ok(())
        }
    }
    common::run(2, async {
        let markers = Arc::new(AtomicU32::new(0));
        let owner = Owner {
            markers: markers.clone(),
            vetoed: false,
        }
        .spawn()
        .supervisor()
        .await
        .unwrap();
        let (tx, rx) = oneshot::channel();
        owner.send(Say::Spawn(tx)).await.unwrap();
        rx.await.unwrap().stop(StopReason::Graceful).await.unwrap();
        let give_up = std::time::Instant::now() + Duration::from_secs(20);
        while markers.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < give_up {
            tokio::task::yield_now().await;
        }
        assert_eq!(markers.load(Ordering::SeqCst), 1);
    });
}
