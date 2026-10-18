//! User values an actor drops during its run. The loop's own drops, a
//! handler's response nobody receives (a notify, or a send whose caller is
//! gone) and the factory of a pruned child, are caught, and a panic in one
//! stops the actor as a handler's panic does: `on_stopped` runs, the status
//! reaches `Stopped`, and the status plane, `wait_stopped` and the
//! supervisor's report carry one reason, `Failure(Panic(..))`. A factory a
//! supervision call drops (`delete_child` in `handle` or `pre_stop`) panics
//! inside that callback and is the callback's panic.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorResult, ActorStatus, ActorSystem, AskError, ChildEvent,
    RestartType, StopReason, SupervisionAction, SupervisionError,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("loop-drops-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {BOUND:?} waiting for: {what}"),
    }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    within(what, async {
        while !cond() {
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
}

fn panic_reason(text: &str) -> StopReason {
    StopReason::Failure(ActorError::Panic(text.to_string()))
}

const RESPONSE_PANIC: &str = "the response's drop panics";
const FACTORY_PANIC: &str = "the factory's capture panics when dropped";

/// A handler's response whose drop panics.
pub struct Bomb;

impl Drop for Bomb {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            panic!("{RESPONSE_PANIC}");
        }
    }
}

/// Holds a handler until the test opens it.
#[derive(Default)]
struct Gate {
    held: AtomicBool,
    entered: AtomicBool,
}

struct Subject {
    gate: Option<Arc<Gate>>,
    on_stopped_ran: Arc<AtomicBool>,
}

impl Actor for Subject {
    type Message = ();
    type Response = Bomb;

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<Bomb> {
        if let Some(gate) = &self.gate {
            gate.entered.store(true, Ordering::SeqCst);
            while gate.held.load(Ordering::SeqCst) {
                sleep(Duration::from_millis(1)).await;
            }
        }
        Ok(Bomb)
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.on_stopped_ran.store(true, Ordering::SeqCst);
        Ok(())
    }
}

type Events = Arc<Mutex<Vec<ChildEvent>>>;

/// Supervises one `Temporary` `Subject` and records its reports.
struct Top {
    child: String,
    gate: Option<Arc<Gate>>,
    on_stopped_ran: Arc<AtomicBool>,
    events: Events,
}

impl Actor for Top {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (gate, flag) = (self.gate.clone(), self.on_stopped_ran.clone());
        ctx.spawn_child(move || Subject {
            gate: gate.clone(),
            on_stopped_ran: flag.clone(),
        })
        .named(self.child.clone())
        .restart_type(RestartType::Temporary)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

struct SubjectRig {
    top: tokio_actors::ActorHandle<Top>,
    child: tokio_actors::ActorHandle<Subject>,
    on_stopped_ran: Arc<AtomicBool>,
    events: Events,
}

async fn subject_rig(gate: Option<Arc<Gate>>) -> SubjectRig {
    let child = uname("subject");
    let on_stopped_ran = Arc::new(AtomicBool::new(false));
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let top = Top {
        child: child.clone(),
        gate,
        on_stopped_ran: on_stopped_ran.clone(),
        events: events.clone(),
    }
    .spawn()
    .named(uname("top"))
    .supervisor()
    .await
    .expect("the supervisor spawns");
    let child = ActorSystem::default()
        .get::<Subject>(&child)
        .expect("the child is registered once its supervisor started");
    SubjectRig {
        top,
        child,
        on_stopped_ran,
        events,
    }
}

/// The child ended with `want` everywhere: `wait_stopped`, the plane read
/// through `status()`, and the supervisor's one report.
async fn assert_one_reason(rig: &SubjectRig, want: &StopReason) {
    let reason = within("the child's end", rig.child.wait_stopped()).await;
    assert_eq!(&reason, want, "wait_stopped");
    assert_eq!(rig.child.status(), ActorStatus::Stopped);
    assert!(
        rig.on_stopped_ran.load(Ordering::SeqCst),
        "on_stopped ran, as for a handler's panic"
    );
    wait_until("the supervisor's report", || {
        !rig.events.lock().unwrap().is_empty()
    })
    .await;
    let events = rig.events.lock().unwrap().clone();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(&events[0].reason, want, "the supervisor's report");
    assert_eq!(events[0].action, SupervisionAction::Removed);
    assert!(rig.top.is_alive());
}

#[test]
fn a_notify_whose_response_drop_panics_stops_the_actor_with_one_reason() {
    common::run(2, async {
        let rig = subject_rig(None).await;
        rig.child.notify(()).await.expect("queued");
        assert_one_reason(&rig, &panic_reason(RESPONSE_PANIC)).await;
        let _ = rig.top.stop(StopReason::Graceful).await;
    });
}

#[test]
fn a_send_whose_caller_is_gone_drops_the_response_through_the_catch() {
    common::run(2, async {
        let gate = Arc::new(Gate::default());
        gate.held.store(true, Ordering::SeqCst);
        let rig = subject_rig(Some(gate.clone())).await;
        let asker = rig.child.clone();
        let caller = tokio::spawn(async move {
            let _ = asker.send(()).await;
        });
        wait_until("the handler to begin", || {
            gate.entered.load(Ordering::SeqCst)
        })
        .await;
        // The caller gives up while the handler runs: its reply has nobody
        // to reach.
        caller.abort();
        let joined = within("the caller's end", caller).await;
        assert!(joined.unwrap_err().is_cancelled());
        gate.held.store(false, Ordering::SeqCst);
        assert_one_reason(&rig, &panic_reason(RESPONSE_PANIC)).await;
        let _ = rig.top.stop(StopReason::Graceful).await;
    });
}

/// A value a child factory captures; its drop panics once armed.
struct FactoryBomb(Arc<AtomicBool>);

impl Drop for FactoryBomb {
    fn drop(&mut self) {
        if self.0.load(Ordering::SeqCst) && !std::thread::panicking() {
            panic!("{FACTORY_PANIC}");
        }
    }
}

struct Leaf;

impl Actor for Leaf {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// What a test asks of mid.
enum MidMsg {
    /// A turn held until the test opens the gate.
    Hold,
}

/// Supervises one `Leaf` whose factory captures a `FactoryBomb`.
struct Mid {
    leaf: String,
    leaf_restart: RestartType,
    armed: Arc<AtomicBool>,
    on_stopped_ran: Arc<AtomicBool>,
    leaf_events: Events,
    gate: Arc<Gate>,
}

impl Actor for Mid {
    type Message = MidMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let bomb = FactoryBomb(self.armed.clone());
        ctx.spawn_child(move || {
            let _captured = &bomb;
            Leaf
        })
        .named(self.leaf.clone())
        .restart_type(self.leaf_restart)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, msg: MidMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            MidMsg::Hold => {
                self.gate.entered.store(true, Ordering::SeqCst);
                while self.gate.held.load(Ordering::SeqCst) {
                    sleep(Duration::from_millis(1)).await;
                }
                Ok(())
            }
        }
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.leaf_events.lock().unwrap().push(event.clone());
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.on_stopped_ran.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Supervises one `Temporary` `Mid` and records its reports.
struct Root {
    mid: String,
    leaf: String,
    leaf_restart: RestartType,
    armed: Arc<AtomicBool>,
    on_stopped_ran: Arc<AtomicBool>,
    leaf_events: Events,
    gate: Arc<Gate>,
    events: Events,
}

impl Actor for Root {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let leaf_restart = self.leaf_restart;
        let (leaf, armed, flag, leaf_events, gate) = (
            self.leaf.clone(),
            self.armed.clone(),
            self.on_stopped_ran.clone(),
            self.leaf_events.clone(),
            self.gate.clone(),
        );
        ctx.spawn_child(move || Mid {
            leaf: leaf.clone(),
            leaf_restart,
            armed: armed.clone(),
            on_stopped_ran: flag.clone(),
            leaf_events: leaf_events.clone(),
            gate: gate.clone(),
        })
        .named(self.mid.clone())
        .restart_type(RestartType::Temporary)
        .with_config(ActorConfig::default().supervisor())
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

struct MidRig {
    root: tokio_actors::ActorHandle<Root>,
    mid: tokio_actors::ActorHandle<Mid>,
    leaf: tokio_actors::ActorHandle<Leaf>,
    armed: Arc<AtomicBool>,
    on_stopped_ran: Arc<AtomicBool>,
    leaf_events: Events,
    gate: Arc<Gate>,
    events: Events,
}

async fn mid_rig(leaf_restart: RestartType) -> MidRig {
    let (mid, leaf) = (uname("mid"), uname("leaf"));
    let armed = Arc::new(AtomicBool::new(false));
    let on_stopped_ran = Arc::new(AtomicBool::new(false));
    let leaf_events: Events = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(Gate::default());
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let root = Root {
        mid: mid.clone(),
        leaf: leaf.clone(),
        leaf_restart,
        armed: armed.clone(),
        on_stopped_ran: on_stopped_ran.clone(),
        leaf_events: leaf_events.clone(),
        gate: gate.clone(),
        events: events.clone(),
    }
    .spawn()
    .named(uname("root"))
    .supervisor()
    .await
    .expect("the root spawns");
    wait_until("mid and its leaf to be registered", || {
        ActorSystem::default().get::<Mid>(&mid).is_some()
            && ActorSystem::default().get::<Leaf>(&leaf).is_some()
    })
    .await;
    MidRig {
        root,
        mid: ActorSystem::default().get::<Mid>(&mid).expect("registered"),
        leaf: ActorSystem::default()
            .get::<Leaf>(&leaf)
            .expect("registered"),
        armed,
        on_stopped_ran,
        leaf_events,
        gate,
        events,
    }
}

/// Mid ended with `want`: its `wait_stopped`, its status, its `on_stopped`
/// and the root's one report.
async fn assert_mid_ended_with(rig: &MidRig, want: &StopReason) {
    let reason = within("mid's end", rig.mid.wait_stopped()).await;
    rig.armed.store(false, Ordering::SeqCst);
    assert_eq!(&reason, want, "mid's wait_stopped");
    assert_eq!(rig.mid.status(), ActorStatus::Stopped);
    assert!(
        rig.on_stopped_ran.load(Ordering::SeqCst),
        "mid's on_stopped ran"
    );
    wait_until("the root's report of mid", || {
        !rig.events.lock().unwrap().is_empty()
    })
    .await;
    let reports = rig.events.lock().unwrap().clone();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(&reports[0].reason, want, "the root's report of mid");
}

/// A leaf's end still undelivered when mid's loop ends is delivered by mid's
/// final drain, and the leaf's factory is dropped with mid's teardown. That
/// drop comes before mid's reason is final: caught, its panic makes mid's
/// graceful stop a panic.
#[test]
fn a_factory_dropped_at_the_teardown_makes_a_graceful_stop_a_panic() {
    common::run(2, async {
        let rig = mid_rig(RestartType::Temporary).await;
        rig.armed.store(true, Ordering::SeqCst);
        rig.gate.held.store(true, Ordering::SeqCst);
        rig.mid.notify(MidMsg::Hold).await.expect("queued");
        wait_until("mid's turn to begin", || {
            rig.gate.entered.load(Ordering::SeqCst)
        })
        .await;
        // The leaf ends while mid is in its turn, and mid is asked to stop:
        // the stop is read first at the turn's end, so the leaf's end is
        // left for the final drain.
        rig.leaf.stop(StopReason::Graceful).await.expect("accepted");
        within("the leaf's end", rig.leaf.wait_stopped()).await;
        rig.mid.stop(StopReason::Graceful).await.expect("accepted");
        rig.gate.held.store(false, Ordering::SeqCst);
        assert_mid_ended_with(&rig, &panic_reason(FACTORY_PANIC)).await;
        let leaf_reports = rig.leaf_events.lock().unwrap().clone();
        assert_eq!(leaf_reports.len(), 1, "{leaf_reports:?}");
        assert_eq!(
            leaf_reports[0].action,
            SupervisionAction::SupervisorStopping
        );
        let _ = rig.root.stop(StopReason::Graceful).await;
    });
}

#[test]
fn a_pruned_factory_whose_drop_panics_stops_its_supervisor_with_one_reason() {
    common::run(2, async {
        let rig = mid_rig(RestartType::Temporary).await;
        rig.armed.store(true, Ordering::SeqCst);
        // The leaf ends; mid prunes its `Temporary` spec in its own loop,
        // dropping the factory.
        rig.leaf.stop(StopReason::Graceful).await.expect("accepted");
        assert_mid_ended_with(&rig, &panic_reason(FACTORY_PANIC)).await;
        let leaf_reports = rig.leaf_events.lock().unwrap().clone();
        assert_eq!(
            leaf_reports.len(),
            1,
            "the leaf's end is delivered: {leaf_reports:?}"
        );
        assert_eq!(leaf_reports[0].reason, StopReason::Graceful);
        let _ = rig.root.stop(StopReason::Graceful).await;
    });
}

// ---------------------------------------------------------------------------
// A factory a callback drops
// ---------------------------------------------------------------------------

/// What a test asks of a [`Deleter`].
enum DeleterMsg {
    /// Answers whether every child of the deleter reads neither alive nor
    /// restart-pending. That equals the state `delete_child` accepts only
    /// because nothing in this rig orders a stop of the deleter: a child
    /// whose supervisor is stopping also reads neither, with its fate
    /// present, and `delete_child` refuses it.
    Settled,
    /// A turn held until the test opens the gate.
    Hold(Arc<AtomicBool>, Arc<tokio::sync::Semaphore>),
    /// A turn that deletes the down leaf.
    DeleteLeaf,
}

/// Supervises one `Transient` `Leaf` whose factory captures a `FactoryBomb`;
/// deletes the down leaf from a turn, or from `pre_stop` and then accepts or
/// vetoes.
struct Deleter {
    leaf: String,
    armed: Arc<AtomicBool>,
    /// `Some(veto)`: `pre_stop` deletes the leaf once, then vetoes or accepts.
    delete_in_pre_stop: Option<bool>,
    stopped_with: Arc<Mutex<Option<StopReason>>>,
    /// What the `delete_child` made in `pre_stop` answered.
    deleted_in_pre_stop: Arc<Mutex<Option<Result<(), SupervisionError>>>>,
}

impl Actor for Deleter {
    type Message = DeleterMsg;
    type Response = bool;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let bomb = FactoryBomb(self.armed.clone());
        ctx.spawn_child(move || {
            let _captured = &bomb;
            Leaf
        })
        .named(self.leaf.clone())
        .restart_type(RestartType::Transient)
        .await?;
        Ok(())
    }

    async fn handle(&mut self, msg: DeleterMsg, ctx: &mut ActorContext<Self>) -> ActorResult<bool> {
        match msg {
            DeleterMsg::Settled => Ok(ctx
                .children()
                .iter()
                .all(|child| !child.is_alive && !child.restart_pending)),
            DeleterMsg::Hold(entered, gate) => {
                entered.store(true, Ordering::SeqCst);
                let _permit = gate.acquire().await;
                Ok(true)
            }
            DeleterMsg::DeleteLeaf => ctx
                .delete_child(self.leaf.clone())
                .map(|()| true)
                .map_err(|err| ActorError::user(err.to_string())),
        }
    }

    async fn pre_stop(&mut self, _reason: &StopReason, ctx: &mut ActorContext<Self>) -> bool {
        match self.delete_in_pre_stop.take() {
            Some(veto) => {
                let answer = ctx.delete_child(self.leaf.clone());
                *self.deleted_in_pre_stop.lock().unwrap() = Some(answer);
                !veto
            }
            None => true,
        }
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.stopped_with.lock().unwrap() = Some(reason.clone());
        Ok(())
    }
}

struct DeleterRig {
    deleter: tokio_actors::ActorHandle<Deleter>,
    armed: Arc<AtomicBool>,
    stopped_with: Arc<Mutex<Option<StopReason>>>,
}

/// A deleter whose `Transient` leaf has ended gracefully (its spec kept,
/// down) and whose factory capture is armed.
async fn deleter_rig(delete_in_pre_stop: Option<bool>) -> DeleterRig {
    let leaf = uname("leaf");
    let armed = Arc::new(AtomicBool::new(false));
    let stopped_with = Arc::new(Mutex::new(None));
    let deleter = Deleter {
        leaf: leaf.clone(),
        armed: armed.clone(),
        delete_in_pre_stop,
        stopped_with: stopped_with.clone(),
        deleted_in_pre_stop: Arc::default(),
    }
    .spawn()
    .named(uname("deleter"))
    .supervisor()
    .await
    .expect("the deleter spawns");
    wait_until("the leaf to be registered", || {
        ActorSystem::default().get::<Leaf>(&leaf).is_some()
    })
    .await;
    let leaf = ActorSystem::default()
        .get::<Leaf>(&leaf)
        .expect("registered");
    leaf.stop(StopReason::Graceful).await.expect("accepted");
    within("the leaf's end", leaf.wait_stopped()).await;
    // The leaf's `Stopped` comes before its supervisor records the death:
    // wait until the deleter's ledger reads the leaf down.
    within("the deleter to record the leaf's end", async {
        while !deleter
            .send(DeleterMsg::Settled)
            .await
            .expect("the deleter answers")
        {
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    armed.store(true, Ordering::SeqCst);
    DeleterRig {
        deleter,
        armed,
        stopped_with,
    }
}

/// The deleter ended with the factory's panic, the reason its `on_stopped`
/// received.
async fn assert_deleter_ended_with_the_factory_panic(rig: &DeleterRig) {
    let reason = within("the deleter's end", rig.deleter.wait_stopped()).await;
    rig.armed.store(false, Ordering::SeqCst);
    let want = panic_reason(FACTORY_PANIC);
    assert_eq!(reason, want);
    assert_eq!(
        rig.stopped_with.lock().unwrap().clone(),
        Some(want),
        "on_stopped received the reason the deleter ends with"
    );
}

/// `pre_stop` deletes the down leaf and vetoes: the factory's drop panics
/// inside `pre_stop`, a panic of that callback, which stops the deleter at
/// once whatever `pre_stop` would have answered.
#[test]
fn a_factory_dropped_in_a_vetoing_pre_stop_stops_the_actor_with_its_panic() {
    common::run(2, async {
        let rig = deleter_rig(Some(true)).await;
        rig.deleter
            .stop(StopReason::Graceful)
            .await
            .expect("accepted");
        assert_deleter_ended_with_the_factory_panic(&rig).await;
    });
}

/// `pre_stop` deletes the down leaf and accepts: the deleter ends with the
/// panic, and `on_stopped` receives it.
#[test]
fn a_factory_dropped_in_an_accepting_pre_stop_is_the_reason_on_stopped_receives() {
    common::run(2, async {
        let rig = deleter_rig(Some(false)).await;
        rig.deleter
            .stop(StopReason::Graceful)
            .await
            .expect("accepted");
        assert_deleter_ended_with_the_factory_panic(&rig).await;
    });
}

/// A turn deletes the down leaf: the factory's drop panics inside `handle`,
/// so the waiting caller receives the panic and the deleter stops with it.
#[test]
fn a_factory_dropped_in_a_handler_answers_the_caller_with_its_panic() {
    common::run(2, async {
        let rig = deleter_rig(None).await;
        let reply = rig.deleter.send(DeleterMsg::DeleteLeaf).await;
        assert!(
            matches!(
                &reply,
                Err(AskError::Actor(ActorError::Panic(text))) if text == FACTORY_PANIC
            ),
            "{reply:?}"
        );
        assert_deleter_ended_with_the_factory_panic(&rig).await;
    });
}

// ---------------------------------------------------------------------------
// The factories released at the start of the teardown
// ---------------------------------------------------------------------------

/// Drains a shared channel in `on_stopped` until every sender is gone.
struct Collector {
    rx: Option<tokio::sync::mpsc::UnboundedReceiver<u32>>,
    drained: Arc<Mutex<Vec<u32>>>,
    finished: Arc<AtomicBool>,
}

impl Actor for Collector {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if let Some(rx) = self.rx.as_mut() {
            while let Some(value) = rx.recv().await {
                self.drained.lock().unwrap().push(value);
            }
        }
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Sends one value when it starts, and holds a sender while it runs.
struct Producer {
    tx: tokio::sync::mpsc::UnboundedSender<u32>,
}

impl Actor for Producer {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _ = self.tx.send(1);
        Ok(())
    }
}

/// Starts a collector, then a producer whose factory holds a sender of the
/// collector's channel. The supervisor itself holds no sender once both
/// started.
struct Pipeline {
    rx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<u32>>>>,
    tx: Option<tokio::sync::mpsc::UnboundedSender<u32>>,
    drained: Arc<Mutex<Vec<u32>>>,
    finished: Arc<AtomicBool>,
}

impl Actor for Pipeline {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (rx, drained, finished) =
            (self.rx.clone(), self.drained.clone(), self.finished.clone());
        ctx.spawn_child(move || Collector {
            rx: rx.lock().unwrap().take(),
            drained: drained.clone(),
            finished: finished.clone(),
        })
        .named(uname("collector"))
        .shutdown(tokio_actors::Shutdown::Timeout(Duration::from_secs(60)))
        .await
        .expect("the collector starts");
        let tx = self.tx.take().expect("one start");
        ctx.spawn_child(move || Producer { tx: tx.clone() })
            .named(uname("producer"))
            .shutdown(tokio_actors::Shutdown::Timeout(Duration::from_secs(60)))
            .await
            .expect("the producer starts");
        Ok(())
    }
}

/// A collector started first drains a shared channel in `on_stopped` until
/// every sender is gone, and a newer producer's factory holds a sender. The
/// teardown releases every factory at its start, so once the producer has
/// stopped no sender is left: the collector's drain ends, and the supervisor
/// stops gracefully, well within the collector's shutdown bound.
#[test]
fn a_collector_whose_senders_a_factory_holds_drains_within_its_stop() {
    common::run(2, async {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let drained = Arc::new(Mutex::new(Vec::new()));
        let finished = Arc::new(AtomicBool::new(false));
        let pipeline = Pipeline {
            rx: Arc::new(Mutex::new(Some(rx))),
            tx: Some(tx),
            drained: drained.clone(),
            finished: finished.clone(),
        }
        .spawn()
        .named(uname("pipeline"))
        .supervisor()
        .await
        .expect("the pipeline starts");
        pipeline.send(()).await.expect("both children start");
        pipeline.stop(StopReason::Graceful).await.expect("accepted");
        let reason = within("the pipeline's stop", pipeline.wait_stopped()).await;
        assert_eq!(reason, StopReason::Graceful);
        assert!(
            finished.load(Ordering::SeqCst),
            "the collector's drain ended"
        );
        assert_eq!(drained.lock().unwrap().clone(), vec![1]);
    });
}

/// A child death the supervisor has not yet recorded when `pre_stop` runs
/// (the stop lane is read before the death plane) is refused by
/// `delete_child`, and the factory is not dropped: the vetoing `pre_stop`
/// keeps the supervisor running. The deleter holds a turn while the leaf
/// ends and the stop is raised, so both are ready when the turn ends.
#[test]
fn a_death_still_pending_when_pre_stop_deletes_is_refused() {
    common::run(2, async {
        let leaf_name = uname("leaf");
        let armed = Arc::new(AtomicBool::new(false));
        let stopped_with = Arc::new(Mutex::new(None));
        let deleted_in_pre_stop = Arc::new(Mutex::new(None));
        let deleter = Deleter {
            leaf: leaf_name.clone(),
            armed: armed.clone(),
            delete_in_pre_stop: Some(true),
            stopped_with: stopped_with.clone(),
            deleted_in_pre_stop: deleted_in_pre_stop.clone(),
        }
        .spawn()
        .named(uname("deleter"))
        .supervisor()
        .await
        .expect("the deleter spawns");
        wait_until("the leaf to be registered", || {
            ActorSystem::default().get::<Leaf>(&leaf_name).is_some()
        })
        .await;
        let leaf = ActorSystem::default()
            .get::<Leaf>(&leaf_name)
            .expect("registered");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let entered = Arc::new(AtomicBool::new(false));
        let held = {
            let deleter = deleter.clone();
            let (entered, gate) = (entered.clone(), gate.clone());
            tokio::spawn(async move { deleter.send(DeleterMsg::Hold(entered, gate)).await })
        };
        wait_until("the deleter to hold its turn", || {
            entered.load(Ordering::SeqCst)
        })
        .await;
        leaf.stop(StopReason::Graceful).await.expect("accepted");
        within("the leaf's end", leaf.wait_stopped()).await;
        armed.store(true, Ordering::SeqCst);
        deleter.stop(StopReason::Graceful).await.expect("accepted");
        gate.add_permits(1);
        within("the held turn", held)
            .await
            .expect("joined")
            .expect("answered");
        // pre_stop ran: its delete_child was refused, and its veto stands.
        wait_until("pre_stop's delete_child", || {
            deleted_in_pre_stop.lock().unwrap().is_some()
        })
        .await;
        let answer = deleted_in_pre_stop.lock().unwrap().take();
        assert!(
            matches!(
                answer,
                Some(Err(
                    SupervisionError::ChildRestarting(_) | SupervisionError::ChildRunning(_)
                ))
            ),
            "the death was still pending, so delete_child was refused: {answer:?}"
        );
        assert!(
            deleter.send(DeleterMsg::Settled).await.is_ok(),
            "the veto stands: the deleter still answers"
        );
        assert!(stopped_with.lock().unwrap().is_none());
        armed.store(false, Ordering::SeqCst);
        deleter.stop(StopReason::Kill).await.expect("accepted");
        within("the deleter's end", deleter.wait_stopped()).await;
    });
}
