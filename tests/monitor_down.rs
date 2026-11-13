#![allow(dead_code)]
//! The notice a monitor delivers: when it is sent, what it names, and where it
//! lands among the watcher's other messages.
//!
//! Every test drives real actors. A `Victim` ends in the way a row names; a
//! `Watcher` (also a supervisor when a row needs one) monitors it and logs
//! what its `handle` and `on_child_stopped` see, in the order they see it.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorId, ActorResult, ActorSystem, ChildEvent, Down,
    DownReason, Incarnation, MonitorId, RestartType, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(300);
/// Rounds a scenario may take to have its priority notice on the lane before
/// the watcher's held turn ends.
const ROUNDS: usize = 20;

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("mdown-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

/// A hold the test opens; a blocked turn or drop polls it with a thread sleep,
/// so it ends at once when the test opens it and ends by itself far past every
/// bound a test awaits.
#[derive(Debug, Default)]
struct Gate {
    open: AtomicBool,
    entered: AtomicBool,
}

impl Gate {
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
    }

    fn block(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let start = std::time::Instant::now();
        while !self.open.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Whether a thread has reached the hold.
    fn is_entered(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// The watched actor
// ---------------------------------------------------------------------------

enum Cmd {
    Fail,
    Panic,
    Hang,
    Block(Arc<Gate>),
    /// Ends the actor from inside its handler with `Shutdown("x")`.
    SelfShutdown,
    Tell(ActorHandle<Watcher>, Vec<u32>),
}

#[derive(Default)]
struct Victim {
    /// Held while the actor value drops, which is after its reason is final.
    drop_gate: Option<Arc<Gate>>,
}

impl Drop for Victim {
    fn drop(&mut self) {
        if let Some(gate) = &self.drop_gate {
            gate.block();
        }
    }
}

impl Actor for Victim {
    type Message = Cmd;
    type Response = ();

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Cmd::SelfShutdown => {
                ctx.stop(StopReason::Shutdown("x".into()))
                    .expect("the lane takes the request");
                Ok(())
            }
            Cmd::Fail => Err(ActorError::user("boom")),
            Cmd::Panic => panic!("boom"),
            Cmd::Hang => {
                std::future::pending::<()>().await;
                Ok(())
            }
            Cmd::Block(gate) => {
                gate.block();
                Ok(())
            }
            Cmd::Tell(watcher, values) => {
                for value in values {
                    let _ = watcher.notify(WMsg::Data(value)).await;
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The watcher
// ---------------------------------------------------------------------------

enum Target {
    Victim(ActorHandle<Victim>),
    Watcher(ActorHandle<Watcher>),
    Id(ActorId),
    Name(String),
}

enum WMsg {
    Watch(Target),
    Gone(Down),
    Data(u32),
    Wait(Arc<Semaphore>),
    /// Adds a permit to `.0` when the turn starts, then holds until `.1` has one.
    Hold(Arc<Semaphore>, Arc<Semaphore>),
    /// Monitors the victim with `monitor_priority`, mapping to `PriGone`.
    WatchPriority(ActorHandle<Victim>),
    PriGone(Down),
    Spawn(String, RestartType),
}

enum Reply {
    Monitor(Option<MonitorId>),
    Child(Option<ActorHandle<Victim>>),
}

#[derive(Debug)]
enum Event {
    /// A notice reached `handle`; `registered` is whether its target's name
    /// was still registered when it did.
    Down {
        down: Down,
        registered: bool,
    },
    Data(u32),
    Child(StopReason),
    /// A notice made by `monitor_priority` reached `handle`.
    PriDown(Down),
}

struct Watcher {
    log: mpsc::UnboundedSender<Event>,
}

impl Actor for Watcher {
    type Message = WMsg;
    type Response = Reply;

    async fn handle(&mut self, msg: WMsg, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        match msg {
            WMsg::Watch(target) => {
                let made = match target {
                    Target::Victim(h) => ctx.monitor(&h, WMsg::Gone),
                    Target::Watcher(h) => ctx.monitor(&h, WMsg::Gone),
                    Target::Id(id) => ctx.monitor(&id, WMsg::Gone),
                    Target::Name(name) => ctx.monitor(name.as_str(), WMsg::Gone),
                };
                Ok(Reply::Monitor(made.ok()))
            }
            WMsg::Gone(down) => {
                let registered = ActorSystem::default()
                    .get_by_id::<Victim>(&down.actor_id)
                    .is_some();
                let _ = self.log.send(Event::Down { down, registered });
                Ok(Reply::Monitor(None))
            }
            WMsg::Data(value) => {
                let _ = self.log.send(Event::Data(value));
                Ok(Reply::Monitor(None))
            }
            WMsg::Wait(gate) => {
                let _ = gate.acquire().await;
                Ok(Reply::Monitor(None))
            }
            WMsg::Hold(entered, gate) => {
                entered.add_permits(1);
                let _ = gate.acquire().await;
                Ok(Reply::Monitor(None))
            }
            WMsg::WatchPriority(victim) => {
                let made = ctx.monitor_priority(&victim, WMsg::PriGone);
                Ok(Reply::Monitor(made.ok()))
            }
            WMsg::PriGone(down) => {
                let _ = self.log.send(Event::PriDown(down));
                Ok(Reply::Monitor(None))
            }
            WMsg::Spawn(name, restart) => {
                let child = ctx
                    .spawn_child(Victim::default)
                    .named(name)
                    .restart_type(restart)
                    // A child held on a thread keeps its place in the subtree
                    // wait for far longer than any window a row observes.
                    .kill_grace(Duration::from_secs(5))
                    .await?;
                Ok(Reply::Child(Some(child)))
            }
        }
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.log.send(Event::Child(event.reason.clone()));
        Ok(())
    }
}

type Log = mpsc::UnboundedReceiver<Event>;

async fn watcher(config: ActorConfig, supervisor: bool) -> (ActorHandle<Watcher>, Log) {
    let (log, rx) = mpsc::unbounded_channel();
    let mut builder = Watcher { log }
        .spawn()
        .named(uname("watcher"))
        .with_config(config);
    if supervisor {
        builder = builder.supervisor();
    }
    (builder.await.expect("the watcher spawns"), rx)
}

async fn victim(drop_gate: Option<Arc<Gate>>) -> ActorHandle<Victim> {
    Victim { drop_gate }
        .spawn()
        .named(uname("victim"))
        .await
        .expect("the victim spawns")
}

async fn watch(w: &ActorHandle<Watcher>, target: Target) -> MonitorId {
    match w.send(WMsg::Watch(target)).await.expect("watcher answers") {
        Reply::Monitor(Some(id)) => id,
        _ => panic!("the monitor was refused"),
    }
}

async fn next_down(log: &mut Log) -> (Down, bool) {
    loop {
        match within("a notice", log.recv()).await {
            Some(Event::Down { down, registered }) => return (down, registered),
            Some(_) => {}
            None => panic!("the watcher's log closed"),
        }
    }
}

async fn assert_quiet(log: &mut Log) {
    sleep(QUIET).await;
    if let Ok(event) = log.try_recv() {
        panic!("unexpected event after the quiet window: {event:?}");
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum End {
    Graceful,
    ErrFailure,
    PanicFailure,
    Kill,
    AbortStuck,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Handle,
    Id,
    Name,
}

fn expected(end: End) -> StopReason {
    match end {
        End::Graceful => StopReason::Graceful,
        End::ErrFailure => StopReason::Failure(ActorError::user("boom")),
        End::PanicFailure => StopReason::Failure(ActorError::Panic("boom".into())),
        End::Kill | End::AbortStuck => StopReason::Kill,
    }
}

async fn end_it(v: &ActorHandle<Victim>, end: End) {
    match end {
        End::Graceful => v.stop(StopReason::Graceful).await.unwrap(),
        End::ErrFailure => v.notify(Cmd::Fail).await.unwrap(),
        End::PanicFailure => v.notify(Cmd::Panic).await.unwrap(),
        End::Kill => v.stop(StopReason::Kill).await.unwrap(),
        End::AbortStuck => {
            v.notify(Cmd::Hang).await.unwrap();
            // A turn parked on a future that never completes: only the
            // abort after the kill grace ends it.
            sleep(Duration::from_millis(30)).await;
            ActorSystem::default()
                .kill_by_id(v.id())
                .await
                .expect("the victim is registered");
        }
    }
}

/// Every way an actor ends, watched by each kind of target: the notice carries
/// the reason the plane carries, the run it was bound to, and arrives after the
/// registered name is free.
#[test]
fn reason_by_target_kind() {
    common::run(4, async {
        let ends = [
            End::Graceful,
            End::ErrFailure,
            End::PanicFailure,
            End::Kill,
            End::AbortStuck,
        ];
        let kinds = [Kind::Handle, Kind::Id, Kind::Name];
        for end in ends {
            for kind in kinds {
                let (w, mut log) = watcher(ActorConfig::default(), false).await;
                let v = victim(None).await;
                let target = match kind {
                    Kind::Handle => Target::Victim(v.clone()),
                    Kind::Id => Target::Id(v.id().clone()),
                    Kind::Name => Target::Name(v.id().as_str().to_string()),
                };
                let id = watch(&w, target).await;
                end_it(&v, end).await;
                let (down, registered) = next_down(&mut log).await;
                let row = format!("{end:?} by {kind:?}");
                assert_eq!(down.monitor, id, "{row}");
                assert_eq!(down.actor_id, *v.id(), "{row}");
                assert_eq!(down.incarnation, Some(v.incarnation()), "{row}");
                assert_eq!(down.reason, DownReason::Stopped(expected(end)), "{row}");
                assert!(!registered, "{row}: the name was still registered");
                assert_quiet(&mut log).await;
            }
        }
    });
}

/// The notice is sent when the plane carries the reason, not when the actor
/// value has dropped: a victim whose value drop is held still notifies. The
/// victim runs on a runtime of its own, so holding its drop holds no task of
/// the watcher's runtime.
#[test]
fn down_does_not_wait_for_the_value_drop() {
    common::run_multi_thread_only(2, async {
        let (w, mut log) = watcher(ActorConfig::default(), false).await;
        let other = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let gate = Arc::new(Gate::default());
        let v = other
            .spawn(victim(Some(gate.clone())))
            .await
            .expect("the victim spawns");
        watch(&w, Target::Victim(v.clone())).await;
        v.stop(StopReason::Graceful).await.unwrap();
        let (down, _) = next_down(&mut log).await;
        assert_eq!(down.reason, DownReason::Stopped(StopReason::Graceful));
        gate.open();
        other.shutdown_background();
    });
}

/// A watcher whose mailbox is full when its target ends still reads the
/// target's earlier messages before the notice.
#[test]
fn down_follows_the_dead_actors_messages() {
    common::run_multi_thread_only(4, async {
        // A priority lane exists so that a notice sent through it would
        // overtake the queue: `monitor` must not use it.
        let config = ActorConfig::default()
            .with_mailbox_capacity(2)
            .with_priority_lane(4);
        let (w, mut log) = watcher(config, false).await;
        let v = victim(None).await;
        watch(&w, Target::Victim(v.clone())).await;
        let gate = Arc::new(Semaphore::new(0));
        let parked = {
            let w = w.clone();
            let gate = gate.clone();
            tokio::spawn(async move { w.send(WMsg::Wait(gate)).await })
        };
        sleep(Duration::from_millis(100)).await;
        v.send(Cmd::Tell(w.clone(), vec![1, 2])).await.unwrap();
        v.stop(StopReason::Graceful).await.unwrap();
        v.wait_stopped().await;
        sleep(Duration::from_millis(100)).await;
        gate.add_permits(1);
        parked.await.unwrap().unwrap();
        let mut order = Vec::new();
        for _ in 0..3 {
            match within("the queued events", log.recv()).await.unwrap() {
                Event::Data(n) => order.push(format!("data {n}")),
                Event::Down { .. } => order.push("down".to_string()),
                Event::Child(_) | Event::PriDown(_) => {}
            }
        }
        assert_eq!(order, ["data 1", "data 2", "down"]);
    });
}

/// One round of the priority notice's scenario with a new watcher and victim:
/// the watcher holds a turn while the victim sends it two messages and ends,
/// the turn ends once both notices hold their slots (one in each lane, the
/// plain one behind the two messages), and the order the watcher handles the
/// four is returned.
async fn priority_notice_round(by_alias: bool) -> Vec<String> {
    let config = ActorConfig::default().with_priority_lane(4);
    let config = if by_alias {
        config.with_priority_by_alias()
    } else {
        config
    };
    let (w, mut log) = watcher(config, false).await;
    let v = victim(None).await;
    watch(&w, Target::Victim(v.clone())).await;
    match w
        .send(WMsg::WatchPriority(v.clone()))
        .await
        .expect("watcher answers")
    {
        Reply::Monitor(Some(_)) => {}
        _ => panic!("the priority monitor was refused"),
    }
    let entered = Arc::new(Semaphore::new(0));
    let gate = Arc::new(Semaphore::new(0));
    let held = {
        let w = w.clone();
        let (entered, gate) = (entered.clone(), gate.clone());
        tokio::spawn(async move { w.send(WMsg::Hold(entered, gate)).await })
    };
    within("the watcher's held turn", entered.acquire())
        .await
        .expect("open")
        .forget();
    v.send(Cmd::Tell(w.clone(), vec![1, 2])).await.unwrap();
    v.stop(StopReason::Graceful).await.unwrap();
    v.wait_stopped().await;
    // Both notices hold their slots: one in each lane, behind the two messages
    // in the mailbox. A length counts a slot that is reserved and not yet
    // sent, so the priority notice may still be on its way to the lane.
    within(
        &format!("both notices to hold their slots, gate {by_alias}"),
        async {
            while w.priority_len() < 1 || w.mailbox_len() < 3 {
                sleep(Duration::from_millis(2)).await;
            }
        },
    )
    .await;
    gate.add_permits(1);
    held.await.unwrap().unwrap();
    let mut order = Vec::new();
    for _ in 0..4 {
        match within("the queued events", log.recv()).await.unwrap() {
            Event::PriDown(_) => order.push("priority down".to_string()),
            Event::Data(n) => order.push(format!("data {n}")),
            Event::Down { .. } => order.push("down".to_string()),
            Event::Child(_) => {}
        }
    }
    order
}

/// `monitor_priority` delivers its notice on the priority lane, ahead of the
/// messages already queued; a plain `monitor` of the same target queues its
/// notice behind them. The watcher holds a turn until both notices and the
/// messages are read off its queue lengths. A length counts a slot that is
/// reserved before the notice is put, so when the turn ends the priority
/// notice may not be on the lane yet: the watcher then takes the first
/// message, the notice follows once it lands, and the mailbox keeps its order.
/// Such a round is run again with new actors; the first round whose notice is
/// on the lane when the turn ends shows the lanes' order.
#[test]
fn a_priority_monitor_notice_is_handled_ahead_of_the_queued_messages() {
    // The row is whether the watcher leaves its lane to priority aliases: the
    // notice of its own priority monitor takes the lane either way.
    for by_alias in [false, true] {
        common::run(4, async move {
            for _ in 0..ROUNDS {
                let order = priority_notice_round(by_alias).await;
                // The notice landed after the turn ended: the first message
                // was taken ahead of it, the notice was handled once, later,
                // and the mailbox's own order is whole.
                let mailbox: Vec<&str> = order
                    .iter()
                    .map(String::as_str)
                    .filter(|entry| *entry != "priority down")
                    .collect();
                let late = order.first().map(String::as_str) == Some("data 1")
                    && order.len() - mailbox.len() == 1
                    && mailbox == ["data 1", "data 2", "down"];
                if late {
                    continue;
                }
                assert_eq!(
                    order,
                    ["priority down", "data 1", "data 2", "down"],
                    "gate {by_alias}"
                );
                return;
            }
            panic!(
                "gate {by_alias}: in no round was the priority notice on the lane \
                 when the held turn ended"
            );
        });
    }
}

/// A monitor is bound to the run it was made on: a restart fires it once with
/// the old run, a name monitor made before the kill is bound to the old run,
/// and one made after follows the new one.
#[test]
fn restart_fires_the_old_entry_once() {
    common::run(4, async {
        let (sup, mut log) = watcher(ActorConfig::default(), true).await;
        let name = uname("restarting");
        let Reply::Child(Some(first)) = sup
            .send(WMsg::Spawn(name.clone(), RestartType::Permanent))
            .await
            .unwrap()
        else {
            panic!("no child");
        };
        let old = first.incarnation();
        let by_handle = watch(&sup, Target::Victim(first.clone())).await;
        let by_name = watch(&sup, Target::Name(name.clone())).await;
        first.stop(StopReason::Kill).await.unwrap();
        let mut downs = Vec::new();
        while downs.len() < 2 {
            downs.push(next_down(&mut log).await.0);
        }
        for down in &downs {
            assert_eq!(down.incarnation, Some(old));
            assert_eq!(down.reason, DownReason::Stopped(StopReason::Kill));
        }
        let fired: HashSet<MonitorId> = downs.iter().map(|d| d.monitor).collect();
        assert_eq!(fired, HashSet::from([by_handle, by_name]));
        // The restarted child is a new run under the same id.
        let second = within("the restart", async {
            loop {
                if let Some(h) = ActorSystem::default().get::<Victim>(&name) {
                    if h.incarnation() != old {
                        return h;
                    }
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert_eq!(second.id(), first.id());
        let after = watch(&sup, Target::Name(name.clone())).await;
        second.stop(StopReason::Kill).await.unwrap();
        let (down, _) = next_down(&mut log).await;
        assert_eq!(down.monitor, after);
        assert_eq!(down.incarnation, Some(second.incarnation()));
    });
}

/// No two runs share an incarnation, in one system or across systems and
/// threads.
#[test]
fn incarnations_are_unique_across_systems() {
    let runs: Vec<Vec<Incarnation>> = (0..2)
        .map(|n| {
            std::thread::spawn(move || {
                let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
                let out = collected.clone();
                common::run(2, async move {
                    let sys = ActorSystem::create(uname(&format!("inc-{n}"))).unwrap();
                    let mut keep = Vec::new();
                    for _ in 0..60 {
                        let h = Victim::default().spawn().on_system(&sys).await.unwrap();
                        out.lock().unwrap().push(h.incarnation());
                        keep.push(h);
                    }
                });
                let all = collected.lock().unwrap().clone();
                all
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    let all: Vec<Incarnation> = runs.into_iter().flatten().collect();
    let distinct: HashSet<Incarnation> = all.iter().copied().collect();
    assert_eq!(distinct.len(), all.len(), "two runs shared an incarnation");
}
