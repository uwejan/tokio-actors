#![allow(dead_code)]
//! A request an actor makes to itself from its own task is refused with
//! `AskError::CallingSelf` instead of parking forever, as Erlang/OTP refuses
//! `gen_server:call` to oneself with `calling_self`. Any other caller is never
//! refused: another actor, a task spawned from a callback, a blocking thread,
//! a later incarnation and an ended actor all reach the target, and a cast to
//! oneself with room is not checked. A cast to oneself that would wait for
//! room in its own full queue is refused at once with `Full`. A `sys` call is
//! refused the same way as a request, with `SysError::CallingSelf`.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    sys::SysError,
    ActorConfig, ActorHandle, ActorResult, AliasOptions, AskError, ChildEvent, RestartType,
    StopReason, SupervisionAction,
};

mod common;

const BOUND: Duration = Duration::from_secs(30);
/// A call that has not returned after this long is parked for good.
const PARK: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Send,
    SendPriority,
    SendTimeout,
    SendPriorityTimeout,
    GetStatus,
    SysStatistics,
}

const CALLS: [Call; 6] = [
    Call::Send,
    Call::SendPriority,
    Call::SendTimeout,
    Call::SendPriorityTimeout,
    Call::GetStatus,
    Call::SysStatistics,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Site {
    PreStart,
    OnStarted,
    Handle,
    PreStop,
    OnStopped,
    OnChildStopped,
    FinalDelivery,
}

const SITES: [Site; 7] = [
    Site::PreStart,
    Site::OnStarted,
    Site::Handle,
    Site::PreStop,
    Site::OnStopped,
    Site::OnChildStopped,
    Site::FinalDelivery,
];

#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    CallingSelf,
    Parked,
    Other(String),
}

type Row = Arc<Mutex<Option<Outcome>>>;

/// Makes `call` on `target` and reads how it ended.
async fn make_call<A: Actor<Message = Msg, Response = ()>>(
    target: &ActorHandle<A>,
    call: Call,
) -> Outcome {
    let long = Duration::from_secs(60);
    let ask = |answer: Result<(), AskError>| match answer {
        Err(AskError::CallingSelf) => Outcome::CallingSelf,
        other => Outcome::Other(format!("{other:?}")),
    };
    timeout(PARK, async {
        match call {
            Call::Send => ask(target.send(Msg::Noop).await),
            Call::SendPriority => ask(target.send_priority(Msg::Noop).await),
            Call::SendTimeout => ask(target.send_timeout(Msg::Noop, long).await),
            Call::SendPriorityTimeout => ask(target.send_priority_timeout(Msg::Noop, long).await),
            Call::GetStatus => ask(target.get_status().await.map(|_| ())),
            Call::SysStatistics => match target.sys().timeout(long).statistics().await {
                Err(SysError::CallingSelf) => Outcome::CallingSelf,
                other => Outcome::Other(format!("{other:?}")),
            },
        }
    })
    .await
    .unwrap_or(Outcome::Parked)
}

#[derive(Clone, Copy)]
enum Msg {
    Noop,
    Go,
}

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The call x site table
// ---------------------------------------------------------------------------

/// Makes one call on itself at one site.
struct Prober {
    site: Site,
    call: Call,
    row: Row,
    probed: Arc<Notify>,
    in_handler: Arc<Notify>,
    gate: Arc<Notify>,
}

impl Prober {
    async fn probe(&mut self, at: Site, ctx: &mut ActorContext<Self>) {
        if self.site != at || self.row.lock().unwrap().is_some() {
            return;
        }
        let outcome = make_call(&ctx.self_handle(), self.call).await;
        *self.row.lock().unwrap() = Some(outcome);
        self.probed.notify_one();
    }
}

impl Actor for Prober {
    type Message = Msg;
    type Response = ();

    async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.probe(Site::PreStart, ctx).await;
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.probe(Site::OnStarted, ctx).await;
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Msg::Go = msg {
            match self.site {
                Site::Handle => self.probe(Site::Handle, ctx).await,
                Site::OnChildStopped => {
                    let child = ctx
                        .spawn_child(|| Idle)
                        .restart_type(RestartType::Temporary)
                        .await?;
                    child.stop(StopReason::Cancelled).await.unwrap();
                }
                Site::FinalDelivery => {
                    let child = ctx
                        .spawn_child(|| Idle)
                        .restart_type(RestartType::Temporary)
                        .await?;
                    child.stop(StopReason::Cancelled).await.unwrap();
                    child.wait_stopped().await;
                    self.in_handler.notify_one();
                    self.gate.notified().await;
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, ctx: &mut ActorContext<Self>) -> bool {
        self.probe(Site::PreStop, ctx).await;
        true
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.probe(Site::OnStopped, ctx).await;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let at = if event.action == SupervisionAction::SupervisorStopping {
            Site::FinalDelivery
        } else {
            Site::OnChildStopped
        };
        self.probe(at, ctx).await;
        Ok(())
    }
}

async fn self_call_row(site: Site, call: Call, priority_lane: bool) -> Outcome {
    let row: Row = Arc::default();
    let probed = Arc::new(Notify::new());
    let in_handler = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let mut config = ActorConfig::default();
    if priority_lane {
        config = config.with_priority_lane(4);
    }
    if matches!(site, Site::OnChildStopped | Site::FinalDelivery) {
        config = config.supervisor();
    }
    let actor = Prober {
        site,
        call,
        row: row.clone(),
        probed: probed.clone(),
        in_handler: in_handler.clone(),
        gate: gate.clone(),
    }
    .spawn()
    .with_config(config)
    .await
    .unwrap();

    match site {
        Site::PreStart | Site::OnStarted => {}
        Site::Handle | Site::OnChildStopped => {
            actor.notify(Msg::Go).await.unwrap();
        }
        Site::PreStop | Site::OnStopped => {
            actor.stop(StopReason::Graceful).await.unwrap();
        }
        Site::FinalDelivery => {
            actor.notify(Msg::Go).await.unwrap();
            timeout(BOUND, in_handler.notified())
                .await
                .expect("the handler holds the loop with a child death pending");
            actor.stop(StopReason::Graceful).await.unwrap();
            gate.notify_one();
        }
    }
    timeout(BOUND, probed.notified())
        .await
        .unwrap_or_else(|_| panic!("{site:?} {call:?}: the site never ran"));
    let outcome = row.lock().unwrap().clone().expect("a row is recorded");
    // The sites that ran before the end stop the actor here; the others
    // already asked, and a second request to an ended actor is refused.
    let _ = actor.stop(StopReason::Graceful).await;
    let _ = timeout(BOUND, actor.wait_stopped()).await;
    outcome
}

async fn every_self_call_is_refused_at_every_site() {
    for site in SITES {
        for call in CALLS {
            for priority_lane in [true, false] {
                let outcome = self_call_row(site, call, priority_lane).await;
                assert_eq!(
                    outcome,
                    Outcome::CallingSelf,
                    "{call:?} from {site:?} (priority lane: {priority_lane})"
                );
            }
        }
    }
}

#[test]
fn every_self_call_is_refused_at_every_site_multi_thread() {
    common::run_multi_thread_only(2, every_self_call_is_refused_at_every_site());
}

// ---------------------------------------------------------------------------
// Callers that are not the actor's own task
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    SpawnedTask,
    BlockingThread,
    CastToSelf,
    /// Fills its own mailbox and lane, then casts to itself once per waiting
    /// cast form.
    CastToFullSelf,
    /// Fills its own mailbox, then casts to itself from a task it spawns and
    /// waits for.
    CastFromSpawnedTask,
}

enum RoamMsg {
    Noop,
    Run(Kind),
    Count,
}

/// Runs a call on itself from a task kind that is not its own task, while its
/// own handler waits for that task, so the request can only be enqueued.
struct Roamer {
    seen: Arc<AtomicU32>,
    row: Arc<Mutex<Vec<String>>>,
}

impl Actor for Roamer {
    type Message = RoamMsg;
    type Response = ();

    async fn handle(&mut self, msg: RoamMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let me = ctx.self_handle();
        let short = Duration::from_millis(150);
        match msg {
            RoamMsg::Noop => {}
            RoamMsg::Count => {
                self.seen.fetch_add(1, Ordering::SeqCst);
            }
            RoamMsg::Run(Kind::SpawnedTask) => {
                let answers = tokio::spawn(async move {
                    vec![
                        me.send_timeout(RoamMsg::Noop, short).await.map(|_| ()),
                        me.send_priority_timeout(RoamMsg::Noop, short)
                            .await
                            .map(|_| ()),
                    ]
                })
                .await
                .unwrap();
                self.row
                    .lock()
                    .unwrap()
                    .extend(answers.iter().map(|a| format!("{a:?}")));
            }
            RoamMsg::Run(Kind::BlockingThread) => {
                let rt = tokio::runtime::Handle::current();
                let answers = tokio::task::spawn_blocking(move || {
                    rt.block_on(async {
                        vec![
                            me.send_timeout(RoamMsg::Noop, short).await.map(|_| ()),
                            me.send_priority_timeout(RoamMsg::Noop, short)
                                .await
                                .map(|_| ()),
                        ]
                    })
                })
                .await
                .unwrap();
                self.row
                    .lock()
                    .unwrap()
                    .extend(answers.iter().map(|a| format!("{a:?}")));
            }
            RoamMsg::Run(Kind::CastToSelf) => {
                me.notify(RoamMsg::Count).await.unwrap();
                me.notify_priority(RoamMsg::Count).await.unwrap();
                me.try_notify(RoamMsg::Count).unwrap();
            }
            RoamMsg::Run(Kind::CastToFullSelf) => {
                while me.try_notify(RoamMsg::Noop).is_ok() {}
                while me.try_notify_priority(RoamMsg::Noop).is_ok() {}
                let plain = ctx.alias(AliasOptions::default());
                let lane = ctx.alias(AliasOptions {
                    priority: true,
                    ..AliasOptions::default()
                });
                let outcome = |answer: Option<String>| answer.unwrap_or_else(|| "Parked".into());
                let rows = [
                    timeout(PARK, me.notify(RoamMsg::Noop))
                        .await
                        .ok()
                        .map(|a| format!("{a:?}")),
                    timeout(PARK, me.notify_priority(RoamMsg::Noop))
                        .await
                        .ok()
                        .map(|a| format!("{a:?}")),
                    timeout(PARK, plain.notify(RoamMsg::Noop))
                        .await
                        .ok()
                        .map(|a| format!("{a:?}")),
                    timeout(PARK, lane.notify_priority(RoamMsg::Noop))
                        .await
                        .ok()
                        .map(|a| format!("{a:?}")),
                ];
                self.row
                    .lock()
                    .unwrap()
                    .extend(rows.into_iter().map(outcome));
            }
            RoamMsg::Run(Kind::CastFromSpawnedTask) => {
                while me.try_notify(RoamMsg::Noop).is_ok() {}
                let (polled_tx, polled_rx) = tokio::sync::oneshot::channel();
                let row = self.row.clone();
                tokio::spawn(async move {
                    let mut cast = std::pin::pin!(me.notify(RoamMsg::Noop));
                    let first = std::future::poll_fn(|cx| {
                        std::task::Poll::Ready(std::future::Future::poll(cast.as_mut(), cx))
                    })
                    .await;
                    let answer = match first {
                        std::task::Poll::Pending => {
                            row.lock().unwrap().push("Pending".to_string());
                            let _ = polled_tx.send(());
                            cast.await
                        }
                        std::task::Poll::Ready(answer) => {
                            row.lock().unwrap().push("Ready".to_string());
                            let _ = polled_tx.send(());
                            answer
                        }
                    };
                    row.lock().unwrap().push(format!("{answer:?}"));
                });
                // The handler returns, and so makes room, only once the task
                // has polled its cast.
                let _ = timeout(BOUND, polled_rx).await;
            }
        }
        Ok(())
    }
}

async fn roamer() -> (ActorHandle<Roamer>, Arc<AtomicU32>, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(AtomicU32::new(0));
    let row: Arc<Mutex<Vec<String>>> = Arc::default();
    let actor = Roamer {
        seen: seen.clone(),
        row: row.clone(),
    }
    .spawn()
    .with_config(ActorConfig::default().with_priority_lane(4))
    .await
    .unwrap();
    (actor, seen, row)
}

/// Both calls are enqueued and time out on the busy handler: a deadline that
/// passed after the request was accepted, never `CallingSelf`.
const REACHED: &str = "Err(Timeout { enqueued: true })";

async fn a_spawned_task_and_a_blocking_thread_are_not_refused() {
    for kind in [Kind::SpawnedTask, Kind::BlockingThread] {
        let (actor, _, row) = roamer().await;
        // The request is acknowledged only once the handler is done.
        let _ = timeout(BOUND, actor.send(RoamMsg::Run(kind))).await;
        assert_eq!(
            *row.lock().unwrap(),
            vec![REACHED.to_string(), REACHED.to_string()],
            "a call from a task that is not the actor's own reached it"
        );
    }
}

async fn a_cast_to_oneself_is_not_checked() {
    let (actor, seen, _) = roamer().await;
    actor.send(RoamMsg::Run(Kind::CastToSelf)).await.unwrap();
    timeout(BOUND, async {
        while seen.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the three casts to oneself were handled");

    // A waiting cast from the actor's own task to its own full mailbox or
    // lane, through the handle or an alias, answers `Full` at once.
    let (actor, _, row) = roamer().await;
    actor
        .send(RoamMsg::Run(Kind::CastToFullSelf))
        .await
        .unwrap();
    assert_eq!(
        *row.lock().unwrap(),
        vec!["Err(Full)".to_string(); 4],
        "notify, notify_priority, an alias's notify and a priority alias's notify_priority"
    );

    // A task the handler spawns and waits for is not the actor's own task:
    // its cast to the full mailbox waits for room, and completes once the
    // handler has returned and the loop takes a message.
    let (actor, _, row) = roamer().await;
    actor
        .send(RoamMsg::Run(Kind::CastFromSpawnedTask))
        .await
        .unwrap();
    timeout(BOUND, async {
        while row.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the spawned task's cast completed");
    assert_eq!(
        *row.lock().unwrap(),
        vec!["Pending".to_string(), "Ok(())".to_string()],
        "a cast from a spawned task waits for room and is then accepted"
    );
}

#[test]
fn a_cast_to_oneself_is_not_checked_multi_thread() {
    common::run_multi_thread_only(2, a_cast_to_oneself_is_not_checked());
}

#[test]
fn a_cast_to_oneself_is_not_checked_current_thread() {
    common::run_current_thread_only(a_cast_to_oneself_is_not_checked());
}

/// Calls its target from its own handler.
struct Caller {
    target: ActorHandle<Idle>,
    row: Row,
}

impl Actor for Caller {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, _msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let outcomes = vec![
            self.target.send(()).await.map(|_| ()),
            self.target.send_timeout((), BOUND).await.map(|_| ()),
            self.target.get_status().await.map(|_| ()),
        ];
        let all_ok = outcomes.iter().all(Result::is_ok);
        *self.row.lock().unwrap() = Some(if all_ok {
            Outcome::Other("reached".into())
        } else {
            Outcome::Other(format!("{outcomes:?}"))
        });
        Ok(())
    }
}

async fn another_actor_is_not_refused() {
    let target = Idle.spawn().await.unwrap();
    let row: Row = Arc::default();
    let caller = Caller {
        target,
        row: row.clone(),
    }
    .spawn()
    .await
    .unwrap();
    timeout(BOUND, caller.send(Msg::Go))
        .await
        .expect("the caller's handler returns")
        .unwrap();
    assert_eq!(
        *row.lock().unwrap(),
        Some(Outcome::Other("reached".into())),
        "calls from another actor's handler reached their target"
    );
}

async fn an_ended_actor_answers_closed_to_every_caller() {
    let actor = Idle.spawn().await.unwrap();
    actor.stop(StopReason::Graceful).await.unwrap();
    actor.wait_stopped().await;
    let from_a_task = {
        let actor = actor.clone();
        tokio::spawn(async move {
            (
                actor.send(()).await.map(|_| ()),
                actor.send_timeout((), BOUND).await.map(|_| ()),
                actor.get_status().await.map(|_| ()),
            )
        })
        .await
        .unwrap()
    };
    for answer in [from_a_task.0, from_a_task.1] {
        assert!(
            matches!(answer, Err(AskError::Closed)),
            "an ended actor answers Closed: {answer:?}"
        );
    }
    assert!(
        matches!(from_a_task.2, Err(AskError::Closed)),
        "{:?}",
        from_a_task.2
    );
}

// ---------------------------------------------------------------------------
// A restart
// ---------------------------------------------------------------------------

type HandleSlot = Arc<Mutex<Option<ActorHandle<Phoenix>>>>;

/// The first incarnation keeps its handle; the second calls it.
struct Phoenix {
    old: HandleSlot,
    first_up: Arc<Notify>,
    row: Row,
    done: Arc<Notify>,
}

impl Actor for Phoenix {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, _msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let earlier = self.old.lock().unwrap().clone();
        match earlier {
            None => {
                *self.old.lock().unwrap() = Some(ctx.self_handle());
                self.first_up.notify_one();
            }
            Some(old) => {
                let outcome = make_call(&old, Call::Send).await;
                *self.row.lock().unwrap() = Some(outcome);
                self.done.notify_one();
            }
        }
        Ok(())
    }
}

struct Parent {
    old: HandleSlot,
    first_up: Arc<Notify>,
    row: Row,
    done: Arc<Notify>,
    child: Arc<Mutex<Option<ActorHandle<Phoenix>>>>,
}

impl Actor for Parent {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, _msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (old, first_up, row, done) = (
            self.old.clone(),
            self.first_up.clone(),
            self.row.clone(),
            self.done.clone(),
        );
        let child = ctx
            .spawn_child(move || Phoenix {
                old: old.clone(),
                first_up: first_up.clone(),
                row: row.clone(),
                done: done.clone(),
            })
            .restart_type(RestartType::Permanent)
            .await?;
        *self.child.lock().unwrap() = Some(child);
        Ok(())
    }
}

async fn restart_new_incarnation_is_not_a_self_call() {
    let (first_up, done) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let row: Row = Arc::default();
    let child: Arc<Mutex<Option<ActorHandle<Phoenix>>>> = Arc::default();
    let parent = Parent {
        old: Arc::default(),
        first_up: first_up.clone(),
        row: row.clone(),
        done: done.clone(),
        child: child.clone(),
    }
    .spawn()
    .supervisor()
    .await
    .unwrap();
    parent.notify(Msg::Go).await.unwrap();
    timeout(BOUND, first_up.notified())
        .await
        .expect("the first incarnation starts");
    // The parent stores the handle when its spawn call returns, which can be
    // after the child's first start has been reported.
    let first = timeout(BOUND, async {
        loop {
            if let Some(handle) = child.lock().unwrap().clone() {
                break handle;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the parent stored the child handle");
    first.stop(StopReason::Cancelled).await.unwrap();
    timeout(BOUND, done.notified())
        .await
        .expect("the second incarnation starts and calls the first");
    let outcome = row.lock().unwrap().clone();
    assert!(
        matches!(&outcome, Some(Outcome::Other(text)) if text == "Err(Closed)"),
        "a later incarnation calling an earlier one reads Closed, not a self-call: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// An actor that monitors itself and asks for its own status
// ---------------------------------------------------------------------------

enum SelfWatchMsg {
    Probe { priority: bool },
    Ignored,
}

struct SelfWatcher {
    row: Row,
}

impl Actor for SelfWatcher {
    type Message = SelfWatchMsg;
    type Response = ();

    async fn handle(&mut self, msg: SelfWatchMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let SelfWatchMsg::Probe { priority } = msg {
            let me = ctx.self_handle();
            let id = if priority {
                ctx.monitor_priority(&me, |_| SelfWatchMsg::Ignored)
            } else {
                ctx.monitor(&me, |_| SelfWatchMsg::Ignored)
            };
            id.expect("a running actor can monitor itself");
            let outcome = match timeout(PARK, me.get_status()).await {
                Err(_) => Outcome::Parked,
                Ok(Err(AskError::CallingSelf)) => Outcome::CallingSelf,
                Ok(other) => Outcome::Other(format!("{other:?}")),
            };
            *self.row.lock().unwrap() = Some(outcome);
        }
        Ok(())
    }
}

/// A self-monitor never parks the actor: the status read from its own task is
/// refused like any self-call, the monitor is in place, and an outside reader
/// sees the monitor counts and the incarnation the handle reports.
async fn a_self_monitor_and_a_status_read_of_oneself() {
    for priority in [false, true] {
        let row: Row = Arc::new(Mutex::new(None));
        let actor = SelfWatcher { row: row.clone() }.spawn().await.unwrap();
        actor.send(SelfWatchMsg::Probe { priority }).await.unwrap();
        assert_eq!(
            row.lock().unwrap().clone(),
            Some(Outcome::CallingSelf),
            "priority {priority}"
        );
        let status = timeout(BOUND, actor.get_status())
            .await
            .expect("the outside read does not park")
            .unwrap();
        assert_eq!(status.monitors, 1, "priority {priority}");
        assert_eq!(status.monitored_by, 1, "priority {priority}");
        assert_eq!(
            status.incarnation,
            actor.incarnation(),
            "priority {priority}"
        );
        actor.stop(StopReason::Graceful).await.unwrap();
        let ended = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the stop completes");
        assert_eq!(ended, StopReason::Graceful, "priority {priority}");
    }
}

// ---------------------------------------------------------------------------
// Work that runs on the actor's thread inside its poll
// ---------------------------------------------------------------------------

/// How the handler runs a call on itself while its own poll holds the thread.
#[derive(Clone, Copy, Debug)]
enum Drive {
    /// The future that `Handle::block_on` drives: the actor's own task.
    InPlace,
    /// A task spawned on the actor's runtime from inside `block_on`.
    SpawnedOuter,
    /// A task of a current-thread runtime of its own, polled on the actor's
    /// thread, inside the actor's poll, under an id of its own.
    SpawnedInner,
    /// Another actor spawned on that inner runtime, which calls this one; the
    /// handler then calls itself once the inner runtime is done.
    ActorInside,
}

const DRIVES: [Drive; 4] = [
    Drive::InPlace,
    Drive::SpawnedOuter,
    Drive::SpawnedInner,
    Drive::ActorInside,
];

enum DriveMsg {
    Noop,
    Run(Drive),
}

type Answers = Arc<Mutex<Vec<String>>>;

/// Calls the driver from inside its poll.
struct Relay {
    target: ActorHandle<Driver>,
    row: Answers,
}

impl Actor for Relay {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let answer = call_noop(self.target.clone()).await;
        self.row.lock().unwrap().push(format!("relay: {answer}"));
        Ok(())
    }
}

async fn call_noop(target: ActorHandle<Driver>) -> String {
    let answer = target
        .send_timeout(DriveMsg::Noop, Duration::from_millis(150))
        .await
        .map(|_| ());
    format!("{answer:?}")
}

struct Driver {
    row: Answers,
}

impl Actor for Driver {
    type Message = DriveMsg;
    type Response = ();

    async fn handle(&mut self, msg: DriveMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let DriveMsg::Run(drive) = msg else {
            return Ok(());
        };
        let me = ctx.self_handle();
        let row = self.row.clone();
        let answers: Vec<String> = tokio::task::block_in_place(|| {
            let outer = tokio::runtime::Handle::current();
            let inner = || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
            };
            match drive {
                Drive::InPlace => outer.block_on(async { vec![call_noop(me.clone()).await] }),
                Drive::SpawnedOuter => outer
                    .block_on(async { vec![tokio::spawn(call_noop(me.clone())).await.unwrap()] }),
                Drive::SpawnedInner => inner()
                    .block_on(async { vec![tokio::spawn(call_noop(me.clone())).await.unwrap()] }),
                Drive::ActorInside => {
                    inner().block_on(async {
                        let relay = Relay {
                            target: me.clone(),
                            row: row.clone(),
                        }
                        .spawn()
                        .await
                        .unwrap();
                        relay.send(()).await.unwrap();
                    });
                    // The inner runtime's polls are over: the outer record
                    // is back, so a call on itself is still its own.
                    let after = outer.block_on(call_noop(me.clone()));
                    vec![format!("after: {after}")]
                }
            }
        });
        self.row.lock().unwrap().extend(answers);
        Ok(())
    }
}

/// The answer of a call the actor makes on itself from its own task.
const SELF_REFUSED: &str = "Err(CallingSelf)";

async fn work_on_the_actors_thread_is_refused_only_when_it_is_the_actors_task() {
    let expected: [(Drive, &[&str]); 4] = [
        (Drive::InPlace, &[SELF_REFUSED]),
        (Drive::SpawnedOuter, &[REACHED]),
        (Drive::SpawnedInner, &[REACHED]),
        (
            Drive::ActorInside,
            &[
                "relay: Err(Timeout { enqueued: true })",
                "after: Err(CallingSelf)",
            ],
        ),
    ];
    assert_eq!(expected.len(), DRIVES.len());
    for (drive, want) in expected {
        let row: Answers = Arc::default();
        let actor = Driver { row: row.clone() }.spawn().await.unwrap();
        timeout(BOUND, actor.send(DriveMsg::Run(drive)))
            .await
            .unwrap_or_else(|_| panic!("{drive:?}: the handler returns"))
            .unwrap();
        let mut got = row.lock().unwrap().clone();
        got.sort_by_key(|answer| !answer.starts_with("relay"));
        assert_eq!(got, want, "{drive:?}");
    }
}

/// Walks across workers: every poll of the actor's task is refused for the
/// actor's own calls, and no other task or actor is, whichever worker the
/// task last ran on.
struct Walker {
    refused: Arc<AtomicU32>,
    other: Arc<AtomicU32>,
}

enum WalkMsg {
    Noop,
    Walk,
}

impl Actor for Walker {
    type Message = WalkMsg;
    type Response = ();

    async fn handle(&mut self, msg: WalkMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if matches!(msg, WalkMsg::Walk) {
            let me = ctx.self_handle();
            for _ in 0..300 {
                tokio::task::yield_now().await;
                match me
                    .send_timeout(WalkMsg::Noop, Duration::from_secs(60))
                    .await
                {
                    Err(AskError::CallingSelf) => self.refused.fetch_add(1, Ordering::SeqCst),
                    _ => self.other.fetch_add(1, Ordering::SeqCst),
                };
            }
        }
        Ok(())
    }
}

/// Calls the walker from another actor's task.
struct Visitor {
    target: ActorHandle<Walker>,
    refused: Arc<AtomicU32>,
}

impl Actor for Visitor {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for _ in 0..300 {
            if let Err(AskError::CallingSelf) = self
                .target
                .send_timeout(WalkMsg::Noop, Duration::from_millis(2))
                .await
            {
                self.refused.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }
}

async fn polls_on_any_worker_match_only_the_actors_own_task() {
    let refused = Arc::new(AtomicU32::new(0));
    let other = Arc::new(AtomicU32::new(0));
    let walker = Walker {
        refused: refused.clone(),
        other: other.clone(),
    }
    .spawn()
    .await
    .unwrap();

    let visitor_refused = Arc::new(AtomicU32::new(0));
    let visitor = Visitor {
        target: walker.clone(),
        refused: visitor_refused.clone(),
    }
    .spawn()
    .await
    .unwrap();
    let mut outsiders = Vec::new();
    for _ in 0..3 {
        let walker = walker.clone();
        outsiders.push(tokio::spawn(async move {
            let mut wrong = 0u32;
            for _ in 0..300 {
                if let Err(AskError::CallingSelf) = walker
                    .send_timeout(WalkMsg::Noop, Duration::from_millis(2))
                    .await
                {
                    wrong += 1;
                }
            }
            wrong
        }));
    }
    let walking = tokio::spawn({
        let walker = walker.clone();
        async move { walker.send(WalkMsg::Walk).await }
    });
    let visiting = tokio::spawn(async move { visitor.send(()).await });

    timeout(BOUND, walking)
        .await
        .expect("the walk ends")
        .unwrap()
        .unwrap();
    timeout(BOUND, visiting)
        .await
        .expect("the visits end")
        .unwrap()
        .unwrap();
    for outsider in outsiders {
        assert_eq!(
            outsider.await.unwrap(),
            0,
            "a task outside the actor is never refused"
        );
    }
    assert_eq!(
        visitor_refused.load(Ordering::SeqCst),
        0,
        "another actor is never refused"
    );
    assert_eq!(
        (refused.load(Ordering::SeqCst), other.load(Ordering::SeqCst)),
        (300, 0),
        "every call the walking handler made on itself, after each hop, was refused"
    );
}

// ---------------------------------------------------------------------------
// Waits on oneself are not refused and do not return
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum SelfWait {
    WaitStopped,
    StopWithin,
    WaitEnded,
}

/// Calls a wait on its own handle from `handle`.
struct SelfWaiter {
    entered: Arc<Notify>,
    returned: Arc<AtomicBool>,
}

impl Actor for SelfWaiter {
    type Message = SelfWait;
    type Response = ();

    async fn handle(&mut self, msg: SelfWait, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let me = ctx.self_handle();
        self.entered.notify_one();
        match msg {
            SelfWait::WaitStopped => {
                me.wait_stopped().await;
            }
            SelfWait::StopWithin => {
                let _ = me
                    .stop_within(StopReason::Graceful, Duration::from_millis(100))
                    .await;
            }
            SelfWait::WaitEnded => {
                me.observe_subtree().wait_ended().await;
            }
        }
        self.returned.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// `wait_stopped`, `stop_within` and an observer's `wait_ended`, called on
/// oneself from a handler, are not refused and never return to the handler:
/// the end each waits for cannot come while the handler waits. Only the
/// ladder's `Kill` and the abort end the actor, and they drop the call.
async fn a_wait_on_oneself_is_not_refused_and_never_returns() {
    for wait in [
        SelfWait::WaitStopped,
        SelfWait::StopWithin,
        SelfWait::WaitEnded,
    ] {
        let entered = Arc::new(Notify::new());
        let returned = Arc::new(AtomicBool::new(false));
        let actor = SelfWaiter {
            entered: entered.clone(),
            returned: returned.clone(),
        }
        .spawn()
        .await
        .unwrap();
        actor.notify(wait).await.unwrap();
        timeout(BOUND, entered.notified())
            .await
            .expect("the handler starts");
        // Past the 100 ms window and the kill grace of the `stop_within` row,
        // and long enough for a refusal to have come back to the handler.
        // The `stop_within` row ends by itself and waits up to the hang bound;
        // the rows that never end wait out the park.
        let park = if matches!(wait, SelfWait::StopWithin) {
            BOUND
        } else {
            PARK
        };
        let ended = timeout(park, actor.wait_stopped()).await;
        match wait {
            SelfWait::StopWithin => assert_eq!(
                ended.expect("the abort ends the actor"),
                StopReason::Kill,
                "{wait:?}: only the Kill and the abort end it"
            ),
            _ => {
                assert!(ended.is_err(), "{wait:?}: the actor is still waiting");
                actor.stop(StopReason::Kill).await.unwrap();
                let reason = timeout(BOUND, actor.wait_stopped())
                    .await
                    .expect("the Kill ends the actor");
                assert_eq!(reason, StopReason::Kill, "{wait:?}");
            }
        }
        assert!(
            !returned.load(Ordering::SeqCst),
            "{wait:?}: the call returned to the handler"
        );
    }
}

#[test]
fn a_wait_on_oneself_is_not_refused_and_never_returns_multi_thread() {
    common::run_multi_thread_only(2, a_wait_on_oneself_is_not_refused_and_never_returns());
}
