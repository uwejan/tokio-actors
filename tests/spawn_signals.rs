#![allow(dead_code)]
//! Monitored and linked spawn on the root builder: what the spawning actor is
//! told, and when.
//!
//! Every test drives real actors. An `Owner` spawns a `Kid` through the form a
//! row names and logs every notice that reaches its `handle`; the test reads
//! the spawn's answer and the log. The rows run on both runtime flavors
//! (`TOKIO_ACTORS_TEST_FLAVOR=current_thread` selects the single-threaded one).
//!
//! Erlang/OTP: `spawn_monitor/1` and `spawn_link/1` (erlang.erl, OTP-28.3.1).

use std::future::{pending, poll_fn, Future, IntoFuture};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, timeout};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorSystem, Down, DownReason, LinkExit, MonitorId,
    RestartType, SpawnError, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(300);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("sigspawn-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

// ---------------------------------------------------------------------------
// The spawned actor
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Stops itself with `Graceful` as soon as it has started.
    EndAtOnce,
    /// Stops itself with a `Failure` as soon as it has started.
    FailAtOnce,
    /// Sends itself an exit signal carrying a `Failure` as soon as it has
    /// started: it ends at its next turn boundary without running a hook, so
    /// nothing yields between the ack and its end.
    ExitSelfAtOnce,
    /// Links to the plan's peer as soon as it has started and runs until
    /// stopped: when the peer fails, the exit signal ends it with a `Kill`
    /// that carries the peer's reason.
    LinkedToPeer,
    /// Runs until stopped.
    StayUp,
    /// `pre_start` returns `Err`.
    FailInit,
    /// `pre_start` never completes.
    NeverStarts,
}

struct Kid {
    mode: Mode,
    /// Counts the runs of `pre_start`, across every kid of a test.
    started: Arc<AtomicUsize>,
    /// Sent when the actor value drops, which is after its reason is final.
    gone: Option<mpsc::UnboundedSender<()>>,
    /// When set, `FailInit` fails only after a permit is added.
    hold: Option<Arc<Semaphore>>,
    /// When set, `pre_start` waits for a permit before it does anything, so
    /// the start is not acknowledged until the test adds one.
    gate: Option<Arc<Semaphore>>,
    /// The actor `LinkedToPeer` links to, and the signal that it has.
    peer: Option<(ActorHandle<Kid>, mpsc::UnboundedSender<()>)>,
}

impl Kid {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            started: Arc::new(AtomicUsize::new(0)),
            gone: None,
            hold: None,
            gate: None,
            peer: None,
        }
    }
}

impl Drop for Kid {
    fn drop(&mut self) {
        if let Some(gone) = &self.gone {
            let _ = gone.send(());
        }
    }
}

impl Actor for Kid {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(gate) = &self.gate {
            gate.acquire().await.expect("the gate stays open").forget();
        }
        self.started.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            Mode::FailInit => {
                if let Some(hold) = &self.hold {
                    hold.acquire().await.expect("the hold stays open").forget();
                }
                Err(ActorError::user("no start"))
            }
            Mode::NeverStarts => {
                pending::<()>().await;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match self.mode {
            Mode::EndAtOnce => ctx
                .stop(StopReason::Graceful)
                .expect("the lane takes the request"),
            Mode::FailAtOnce => ctx
                .stop(StopReason::Failure(ActorError::user("fails after the ack")))
                .expect("the lane takes the request"),
            Mode::LinkedToPeer => {
                let (peer, linked) = self.peer.as_ref().expect("the plan names a peer");
                ctx.link(peer).expect("the kid runs");
                let _ = linked.send(());
            }
            Mode::ExitSelfAtOnce => {
                let me = ctx.actor_id().clone();
                ctx.exit(
                    &me,
                    StopReason::Failure(ActorError::user("fails after the ack")),
                )
                .expect("an exit signal is refused only at its target's bound");
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The owner
// ---------------------------------------------------------------------------

/// The form of spawn a row uses.
#[derive(Clone, Copy, Debug)]
enum How {
    Monitored,
    MonitoredLinked,
    Linked,
}

#[derive(Clone)]
struct Plan {
    how: How,
    mode: Mode,
    name: Option<String>,
    start_timeout: Option<Duration>,
    /// Whether the spawn skips the start acknowledgment.
    detached: bool,
    started: Arc<AtomicUsize>,
    gone: Option<mpsc::UnboundedSender<()>>,
    gate: Option<Arc<Semaphore>>,
    peer: Option<(ActorHandle<Kid>, mpsc::UnboundedSender<()>)>,
}

impl Plan {
    fn new(how: How, mode: Mode) -> Self {
        Self {
            how,
            mode,
            name: None,
            start_timeout: None,
            detached: false,
            started: Arc::new(AtomicUsize::new(0)),
            gone: None,
            gate: None,
            peer: None,
        }
    }

    fn kid(&self) -> Kid {
        Kid {
            mode: self.mode,
            started: self.started.clone(),
            gone: self.gone.clone(),
            hold: None,
            gate: self.gate.clone(),
            peer: self.peer.clone(),
        }
    }
}

enum Msg {
    Spawn(Plan),
    /// A plain `ctx.spawn_child` then `ctx.monitor`, the pattern a supervised
    /// child uses.
    SpawnChildThenMonitor(String),
    Demonitor(MonitorId),
    /// A spawn whose kid ends before the owner resumes from the start ack
    /// (the plan names a gate; the receiver reports the kid's drop).
    SpawnLate(Plan, Mutex<Option<mpsc::UnboundedReceiver<()>>>),
    Ping,
    Gone(Down),
    Exit(LinkExit),
}

enum Reply {
    Spawned(Result<(ActorHandle<Kid>, Option<MonitorId>), SpawnError>),
    Child(ActorHandle<Kid>, MonitorId),
    /// The answer of a late spawn, and what `unlink` answered for the kid
    /// right after it.
    Late(
        Result<(ActorHandle<Kid>, Option<MonitorId>), SpawnError>,
        bool,
    ),
    Revoked(bool),
    Pong,
}

#[derive(Debug)]
enum Event {
    Down(Down),
    Exit(LinkExit),
    /// What a spawn made from `on_stopped` answered.
    StoppedSpawn(Result<(), SpawnError>),
}

struct Owner {
    log: mpsc::UnboundedSender<Event>,
    /// A spawn to attempt from `on_stopped`.
    at_stop: Option<Plan>,
    /// Whether the owner reads linked exits as messages.
    trap: bool,
}

async fn run_plan(
    plan: &Plan,
    ctx: &mut ActorContext<Owner>,
) -> Result<(ActorHandle<Kid>, Option<MonitorId>), SpawnError> {
    let mut builder = plan.kid().spawn();
    if let Some(name) = &plan.name {
        builder = builder.named(name.clone());
    }
    if let Some(bound) = plan.start_timeout {
        builder = builder.start_timeout(bound);
    }
    if plan.detached {
        builder = builder.detached();
    }
    match plan.how {
        How::Monitored => builder
            .monitored(ctx, Msg::Gone)
            .await
            .map(|(h, id)| (h, Some(id))),
        How::MonitoredLinked => builder
            .monitored(ctx, Msg::Gone)
            .linked()
            .await
            .map(|(h, id)| (h, Some(id))),
        How::Linked => builder.linked(ctx).await.map(|h| (h, None)),
    }
}

/// Polls the spawn once, which starts the kid and parks on its ack (the kid's
/// gate keeps the ack back, so the first poll is always pending), then lets
/// the kid start, end and drop before the spawn is polled again.
async fn late<F: IntoFuture>(
    spawn: F,
    gate: Arc<Semaphore>,
    mut gone: mpsc::UnboundedReceiver<()>,
) -> F::Output {
    let mut spawn = Box::pin(spawn.into_future());
    poll_fn(|cx| {
        assert!(spawn.as_mut().poll(cx).is_pending(), "the ack is held back");
        Poll::Ready(())
    })
    .await;
    gate.add_permits(1);
    gone.recv().await.expect("the kid drops");
    spawn.await
}

impl Actor for Owner {
    type Message = Msg;
    type Response = Reply;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.trap {
            ctx.trap_exit(Msg::Exit);
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        match msg {
            Msg::Spawn(plan) => Ok(Reply::Spawned(run_plan(&plan, ctx).await)),
            Msg::SpawnChildThenMonitor(name) => {
                // The child's first start fails only once the monitor exists.
                let hold = Arc::new(Semaphore::new(0));
                let factory_hold = hold.clone();
                let child = ctx
                    .spawn_child(move || {
                        let mut kid = Kid::new(Mode::FailInit);
                        kid.hold = Some(factory_hold.clone());
                        kid
                    })
                    .named(name)
                    .restart_type(RestartType::Temporary)
                    .await?;
                let id = ctx.monitor(&child, Msg::Gone).expect("the owner runs");
                hold.add_permits(1);
                Ok(Reply::Child(child, id))
            }
            Msg::SpawnLate(plan, gone) => {
                let gone = gone.lock().unwrap().take().expect("sent once");
                let gate = plan.gate.clone().expect("a late spawn has a gate");
                let builder = plan.kid().spawn();
                let answer = match plan.how {
                    How::Linked => late(builder.linked(ctx), gate, gone)
                        .await
                        .map(|h| (h, None)),
                    _ => late(builder.monitored(ctx, Msg::Gone).linked(), gate, gone)
                        .await
                        .map(|(h, id)| (h, Some(id))),
                };
                // The kid ended before the link was made, so its exit is a
                // fire that already won: the owner cannot take it back.
                let unlinked = answer.as_ref().is_ok_and(|(kid, _)| ctx.unlink(kid));
                Ok(Reply::Late(answer, unlinked))
            }
            Msg::Demonitor(id) => Ok(Reply::Revoked(ctx.demonitor(id))),
            Msg::Ping => Ok(Reply::Pong),
            Msg::Gone(down) => {
                let _ = self.log.send(Event::Down(down));
                Ok(Reply::Pong)
            }
            Msg::Exit(exit) => {
                let _ = self.log.send(Event::Exit(exit));
                Ok(Reply::Pong)
            }
        }
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if let Some(plan) = self.at_stop.take() {
            let answer = run_plan(&plan, ctx).await.map(|_| ());
            let _ = self.log.send(Event::StoppedSpawn(answer));
        }
        Ok(())
    }
}

type Log = mpsc::UnboundedReceiver<Event>;

async fn owner(trap: bool, supervisor: bool, at_stop: Option<Plan>) -> (ActorHandle<Owner>, Log) {
    let (log, rx) = mpsc::unbounded_channel();
    let mut builder = Owner { log, at_stop, trap }.spawn().named(uname("owner"));
    if supervisor {
        builder = builder.supervisor();
    }
    (builder.await.expect("the owner spawns"), rx)
}

async fn spawn_via(
    owner: &ActorHandle<Owner>,
    plan: Plan,
) -> Result<(ActorHandle<Kid>, Option<MonitorId>), SpawnError> {
    match owner.send(Msg::Spawn(plan)).await.expect("owner answers") {
        Reply::Spawned(answer) => answer,
        _ => panic!("unexpected reply"),
    }
}

async fn next_event(log: &mut Log) -> Event {
    within("an event", log.recv())
        .await
        .expect("the owner's log closed")
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

/// A kid that ends at once after its ack is never missed: one `Down` arrives,
/// after the spawn's answer, carrying the id the spawn returned and the run it
/// was bound to. A kid that stays up is monitored under an id that revokes.
/// A spawn that also links keeps the same notice.
#[test]
fn monitored_spawn_fires_after_the_ack() {
    common::run(4, async {
        // Rows: the form of spawn, with a kid that ends at once.
        // The kid may end before the monitor is made or after: the notice
        // carries its own reason either way, so the rows repeat.
        for round in 0..40 {
            let how = if round % 2 == 0 {
                How::Monitored
            } else {
                How::MonitoredLinked
            };
            let (o, mut log) = owner(false, false, None).await;
            let (kid, id) = spawn_via(&o, Plan::new(how, Mode::EndAtOnce))
                .await
                .expect("the kid starts");
            let id = id.expect("a monitored spawn returns its id");
            let Event::Down(down) = next_event(&mut log).await else {
                panic!("{how:?}: the first event is not a Down");
            };
            assert_eq!(down.monitor, id, "{how:?}: the Down names the returned id");
            assert_eq!(down.actor_id, *kid.id(), "{how:?}");
            assert_eq!(down.incarnation, Some(kid.incarnation()), "{how:?}");
            assert_eq!(
                down.reason,
                DownReason::Stopped(StopReason::Graceful),
                "{how:?}"
            );
            if round == 39 {
                assert_quiet(&mut log).await;
            }
        }

        // Row: a kid that stays up; the returned id revokes, and no notice
        // follows the kid's end.
        let (o, mut log) = owner(false, false, None).await;
        let (kid, id) = spawn_via(&o, Plan::new(How::Monitored, Mode::StayUp))
            .await
            .expect("the kid starts");
        let id = id.expect("a monitored spawn returns its id");
        match o.send(Msg::Demonitor(id)).await.expect("owner answers") {
            Reply::Revoked(revoked) => assert!(revoked, "the returned id revokes the monitor"),
            _ => panic!("unexpected reply"),
        }
        kid.stop(StopReason::Graceful).await.unwrap();
        kid.wait_stopped().await;
        assert_quiet(&mut log).await;
    });
}

/// A start that fails has one outcome, the `Err`: no `Down` ever reaches the
/// owner, and the owner holds no monitor.
#[test]
fn failed_start_fires_nothing() {
    common::run(4, async {
        for how in [How::Monitored, How::MonitoredLinked] {
            // Row: `pre_start` returns `Err`.
            let (o, mut log) = owner(true, false, None).await;
            let (gone_tx, mut gone) = mpsc::unbounded_channel();
            let mut plan = Plan::new(how, Mode::FailInit);
            plan.gone = Some(gone_tx);
            let answer = spawn_via(&o, plan).await;
            assert!(
                matches!(answer, Err(SpawnError::Init(_))),
                "{how:?} pre_start Err: {answer:?}"
            );
            within("the failed kid's drop", gone.recv()).await;
            assert_quiet(&mut log).await;
            assert_eq!(o.get_status().await.unwrap().monitors, 0, "{how:?}");

            // Row: the start bound expires.
            let (o, mut log) = owner(true, false, None).await;
            let (gone_tx, mut gone) = mpsc::unbounded_channel();
            let mut plan = Plan::new(how, Mode::NeverStarts);
            plan.start_timeout = Some(Duration::from_millis(100));
            plan.gone = Some(gone_tx);
            let answer = spawn_via(&o, plan).await;
            assert!(
                matches!(answer, Err(SpawnError::StartTimeout)),
                "{how:?} start_timeout: {answer:?}"
            );
            within("the cut kid's drop", gone.recv()).await;
            assert_quiet(&mut log).await;
            assert_eq!(o.get_status().await.unwrap().monitors, 0, "{how:?}");

            // Row: the name is taken.
            let (o, mut log) = owner(true, false, None).await;
            let name = uname("taken");
            let holder = Kid::new(Mode::StayUp)
                .spawn()
                .named(name.clone())
                .await
                .expect("the holder starts");
            let (gone_tx, mut gone) = mpsc::unbounded_channel();
            let mut plan = Plan::new(how, Mode::StayUp);
            plan.name = Some(name);
            plan.gone = Some(gone_tx);
            let answer = spawn_via(&o, plan).await;
            assert!(
                matches!(answer, Err(SpawnError::NameTaken { .. })),
                "{how:?} name taken: {answer:?}"
            );
            within("the refused kid's drop", gone.recv()).await;
            holder.stop(StopReason::Graceful).await.unwrap();
            holder.wait_stopped().await;
            assert_quiet(&mut log).await;
            assert_eq!(o.get_status().await.unwrap().monitors, 0, "{how:?}");
        }
    });
}

/// A start that fails arms no link either: a trapping owner reads no exit.
#[test]
fn failed_start_arms_no_link() {
    common::run(4, async {
        for how in [How::Linked, How::MonitoredLinked] {
            // Row: `pre_start` returns `Err`.
            let (o, mut log) = owner(true, false, None).await;
            let (gone_tx, mut gone) = mpsc::unbounded_channel();
            let mut plan = Plan::new(how, Mode::FailInit);
            plan.gone = Some(gone_tx);
            assert!(matches!(
                spawn_via(&o, plan).await,
                Err(SpawnError::Init(_))
            ));
            within("the failed kid's drop", gone.recv()).await;
            assert_quiet(&mut log).await;

            // Row: the start bound expires.
            let (o, mut log) = owner(true, false, None).await;
            let (gone_tx, mut gone) = mpsc::unbounded_channel();
            let mut plan = Plan::new(how, Mode::NeverStarts);
            plan.start_timeout = Some(Duration::from_millis(100));
            plan.gone = Some(gone_tx);
            assert!(matches!(
                spawn_via(&o, plan).await,
                Err(SpawnError::StartTimeout)
            ));
            within("the cut kid's drop", gone.recv()).await;
            assert_quiet(&mut log).await;

            // Row: the name is taken.
            let (o, mut log) = owner(true, false, None).await;
            let name = uname("taken");
            let holder = Kid::new(Mode::StayUp)
                .spawn()
                .named(name.clone())
                .await
                .expect("the holder starts");
            let (gone_tx, mut gone) = mpsc::unbounded_channel();
            let mut plan = Plan::new(how, Mode::StayUp);
            plan.name = Some(name);
            plan.gone = Some(gone_tx);
            assert!(matches!(
                spawn_via(&o, plan).await,
                Err(SpawnError::NameTaken { .. })
            ));
            within("the refused kid's drop", gone.recv()).await;
            holder.stop(StopReason::Graceful).await.unwrap();
            holder.wait_stopped().await;
            assert_quiet(&mut log).await;
        }
    });
}

/// `.monitored(..).linked()` does both: a `Down` and, for a trapping owner,
/// a `LinkExit`, each once. The rows are the spawn that waits for the start
/// acknowledgment and the detached one, which arms both as soon as the task
/// is spawned.
#[test]
fn monitored_then_linked_does_both() {
    for detached in [false, true] {
        common::run(4, async move {
            let (o, mut log) = owner(true, false, None).await;
            let mut plan = Plan::new(How::MonitoredLinked, Mode::EndAtOnce);
            plan.detached = detached;
            let (kid, id) = spawn_via(&o, plan).await.expect("the kid starts");
            let id = id.expect("a monitored spawn returns its id");
            let (mut downs, mut exits) = (0, 0);
            while downs + exits < 2 {
                match next_event(&mut log).await {
                    Event::Down(down) => {
                        assert_eq!(down.monitor, id);
                        assert_eq!(down.incarnation, Some(kid.incarnation()));
                        downs += 1;
                    }
                    Event::Exit(exit) => {
                        assert_eq!(exit.incarnation, Some(kid.incarnation()));
                        exits += 1;
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
            assert_eq!((downs, exits), (1, 1), "detached {detached}");
            assert_quiet(&mut log).await;
        });
    }
}

/// An owner that is stopping takes no registration: each form answers
/// `OwnerStopping` and spawns nothing, not even a named kid whose name would
/// show it.
#[test]
fn owner_stopping_is_refused() {
    common::run(4, async {
        for how in [How::Monitored, How::MonitoredLinked, How::Linked] {
            let name = uname("refused");
            let mut plan = Plan::new(how, Mode::StayUp);
            plan.name = Some(name.clone());
            let started = plan.started.clone();
            let (o, mut log) = owner(false, false, Some(plan)).await;
            o.stop(StopReason::Graceful).await.unwrap();
            let Event::StoppedSpawn(answer) = next_event(&mut log).await else {
                panic!("{how:?}: the first event is not the spawn's answer");
            };
            assert_eq!(answer, Err(SpawnError::OwnerStopping), "{how:?}");
            o.wait_stopped().await;
            assert!(
                ActorSystem::default().get::<Kid>(&name).is_none(),
                "{how:?}: a kid was registered"
            );
            assert_eq!(started.load(Ordering::SeqCst), 0, "{how:?}: a kid started");
        }
    });
}

/// A kid that ends before the owner resumes from the start ack still reaches
/// the owner as a link to a kid that existed before it ran: the kid's own
/// reason goes through the link rules, `Graceful` leaves a non-trapping owner
/// running and arrives as an exit for a trapping one, any other reason ends a
/// non-trapping owner with it. Neither is `NoProc`, which only a plain
/// `ctx.link` to an actor that had ended gives. The kid's gate holds its ack
/// until the owner has polled the spawn once and parked, and the owner then
/// waits for the kid's drop, so the end comes before the link on every run.
/// The third kid ends because a peer it is linked to failed: its `Kill`
/// carries the peer's failure, and the `Down` and the `LinkExit` of one
/// monitored and linked spawn carry that one reason. The exit is a fire that
/// already won, so `unlink` answers false and the exit still arrives.
#[test]
fn a_kid_that_ends_before_the_link_is_made_delivers_its_real_reason() {
    common::run(4, async {
        for how in [How::Linked, How::MonitoredLinked] {
            for trap in [false, true] {
                for mode in [Mode::EndAtOnce, Mode::FailAtOnce, Mode::LinkedToPeer] {
                    let row = format!("{how:?} trap={trap} {mode:?}");
                    let graceful = matches!(mode, Mode::EndAtOnce);
                    let (o, mut log) = owner(trap, false, None).await;
                    let (gone_tx, gone) = mpsc::unbounded_channel();
                    let mut plan = Plan::new(how, mode);
                    plan.gone = Some(gone_tx);
                    plan.gate = Some(Arc::new(Semaphore::new(0)));
                    if matches!(mode, Mode::LinkedToPeer) {
                        // The kid links to a peer once it has started; the
                        // peer then fails, which ends the kid with a `Kill`
                        // that carries the failure, before the owner resumes.
                        let peer = Kid::new(Mode::StayUp).spawn().await.expect("the peer");
                        let (linked_tx, mut linked) = mpsc::unbounded_channel();
                        plan.peer = Some((peer.clone(), linked_tx));
                        tokio::spawn(async move {
                            linked.recv().await.expect("the kid links");
                            let _ = peer
                                .stop(StopReason::Failure(ActorError::user("fails after the ack")))
                                .await;
                        });
                    }
                    let reply = o
                        .send(Msg::SpawnLate(plan, Mutex::new(Some(gone))))
                        .await
                        .expect("the owner answers");
                    let Reply::Late(Ok((kid, _)), unlinked) = reply else {
                        panic!("{row}: the spawn does not answer Ok");
                    };
                    assert!(!unlinked, "{row}: unlink answers false for an ended kid");
                    let want = if graceful {
                        StopReason::Graceful
                    } else {
                        StopReason::Failure(ActorError::user("fails after the ack"))
                    };
                    if !trap && !graceful {
                        match within("the owner's end", o.wait_stopped()).await {
                            StopReason::Failure(err) => assert!(
                                err.to_string().contains("fails after the ack"),
                                "{row}: the owner ends with the kid's failure: {err}"
                            ),
                            other => panic!("{row}: the owner ends with {other:?}"),
                        }
                        continue;
                    }
                    // The owner lives: it reads the kid's end, as a `Down`
                    // for a monitored spawn and as an exit when it traps.
                    let mut downs = 0;
                    let mut exits = 0;
                    let wanted =
                        usize::from(matches!(how, How::MonitoredLinked)) + usize::from(trap);
                    while downs + exits < wanted {
                        match next_event(&mut log).await {
                            Event::Down(down) => {
                                downs += 1;
                                assert_eq!(down.reason, DownReason::Stopped(want.clone()), "{row}");
                            }
                            Event::Exit(exit) => {
                                exits += 1;
                                assert_eq!(exit.actor_id, *kid.id(), "{row}");
                                assert_eq!(exit.incarnation, Some(kid.incarnation()), "{row}");
                                assert_eq!(exit.reason, DownReason::Stopped(want.clone()), "{row}");
                            }
                            other => panic!("{row}: unexpected {other:?}"),
                        }
                    }
                    assert_eq!(exits, usize::from(trap), "{row}: exits");

                    assert_quiet(&mut log).await;
                    assert!(
                        matches!(o.send(Msg::Ping).await, Ok(Reply::Pong)),
                        "{row}: the owner is still running"
                    );
                }
            }
        }
    });
}
