//! The order of an actor's end. On return: a supervisor's child factories
//! drop with its children's teardown, then its name is freed, then its
//! queued messages, its value and its context are dropped, then its status
//! plane reads as ended, then its subtree. Its timers and streams are
//! released from the mailbox's close on, each when its forwarder next runs
//! or by the end's abort, always before the plane reads as ended. On an
//! abort while suspended: the callback in progress drops, then the name is
//! freed, then the queued messages, the context (which ends the timers and
//! streams) and the value, then the plane and the subtree. On an abort
//! before the first poll the plane reads as ended only after the name is
//! freed and everything the actor owned is dropped. A task whose own poll
//! unwinds once its name is freed ends in an abort's order, with the reason
//! of an uncaught panic. No drop of the end unwinds on any path. Each owned
//! item records what it saw at the moment of its own drop.

use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::runtime::{spawn_actor, ActorConfig};
use crate::actor::subtree::SubtreeWatch;
use crate::actor::Actor;
use crate::error::{ActorError, ActorResult};
use crate::system::ActorSystem;
use crate::types::{ActorId, ActorStatus, StatusRx, StopReason, StreamEvent};

type InitAck = oneshot::Receiver<Result<(), ActorError>>;

/// What the test can read about the actor while one of its items drops.
struct Probe {
    status: StatusRx,
    subtree: SubtreeWatch,
    system: Arc<ActorSystem>,
    name: String,
    ack: Mutex<(Option<InitAck>, Option<AckRead>)>,
}

/// How the init ack read at the moment of a drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AckRead {
    /// The actor was spawned without an ack.
    None,
    /// Not answered yet.
    Pending,
    /// Answered with an init error.
    Failed,
    /// Answered with success, or dropped unanswered.
    Other,
}

/// What one owned item saw when it was dropped.
#[derive(Debug, Clone)]
struct Seen {
    who: &'static str,
    plane_ended: bool,
    name_registered: bool,
    subtree_gone: bool,
    hold_released: bool,
    ack: AckRead,
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<Seen>>>);

impl Log {
    fn seen(&self) -> Vec<Seen> {
        self.0.lock().unwrap().clone()
    }

    fn of(&self, who: &str) -> Seen {
        self.seen()
            .into_iter()
            .find(|seen| seen.who == who)
            .unwrap_or_else(|| panic!("{who} was never dropped: {:?}", self.seen()))
    }
}

/// Something the actor owns: it records what it sees when it is dropped, and
/// panics there when asked to.
struct Owned {
    who: &'static str,
    probe: Arc<OnceLock<Probe>>,
    log: Log,
    panics: bool,
    hold: Option<Hold>,
}

/// Holds a drop on its worker thread until the test releases it, and says
/// when the drop began (multi-thread runtimes only).
struct Hold {
    entered: Option<oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}

impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(probe) = self.probe.get() {
            let ack = {
                let mut ack = probe.ack.lock().unwrap();
                let (rx, answered) = &mut *ack;
                match (answered.as_ref(), rx.as_mut()) {
                    (Some(read), _) => *read,
                    (None, None) => AckRead::None,
                    (None, Some(rx)) => match rx.try_recv() {
                        Err(oneshot::error::TryRecvError::Empty) => AckRead::Pending,
                        Ok(Err(_)) => *answered.insert(AckRead::Failed),
                        _ => *answered.insert(AckRead::Other),
                    },
                }
            };
            self.log.0.lock().unwrap().push(Seen {
                who: self.who,
                plane_ended: probe.status.has_changed().is_err()
                    || probe.status.borrow().status == ActorStatus::Stopped,
                name_registered: probe.system.get::<Subject>(&probe.name).is_some(),
                subtree_gone: probe.subtree.is_gone(),
                hold_released: probe.subtree.is_released(),
                ack,
            });
        }
        if let Some(hold) = self.hold.as_mut() {
            if let Some(entered) = hold.entered.take() {
                let _ = entered.send(());
            }
            let release = &hold.release;
            tokio::task::block_in_place(|| {
                let _ = release.recv();
            });
        }
        if self.panics {
            panic!("{} panicked when dropped", self.who);
        }
    }
}

/// A stream that never yields and owns an item.
struct HeldStream(#[allow(dead_code)] Owned);

impl Stream for HeldStream {
    type Item = ();

    fn poll_next(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        Poll::Pending
    }
}

enum Msg {
    /// Holds the turn until released, so messages queue behind it; says
    /// when the turn began.
    Hold(oneshot::Receiver<()>, oneshot::Sender<()>),
    /// Queued and never handled: only its drop counts.
    Queued(#[allow(dead_code)] Owned),
    /// The handler panics.
    Panic,
    Tick,
    Stream,
}

impl From<StreamEvent<()>> for Msg {
    fn from(_: StreamEvent<()>) -> Self {
        Msg::Stream
    }
}

/// Which way the actor's start goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    /// Starts and runs its message loop.
    Run,
    /// `pre_start` returns an error.
    FailInit,
    /// `on_started` parks forever.
    Park,
}

/// What the actor owns besides its value, built when it starts.
struct Extras {
    stream: Option<Owned>,
    timer: Option<Owned>,
    /// Captured by a child's factory: a supervisor holds it until its end.
    factory: Option<Owned>,
    started: Option<oneshot::Sender<()>>,
    /// Awaited before `pre_start` does anything: the test sets its probe
    /// first.
    go: Option<oneshot::Receiver<()>>,
}

/// A child with nothing to do.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

struct Subject {
    _value: Owned,
    start: Start,
    extras: Extras,
}

impl Subject {
    async fn attach(&mut self, ctx: &mut ActorContext<Self>) {
        if let Some(owned) = self.extras.stream.take() {
            ctx.add_stream(HeldStream(owned));
        }
        if let Some(owned) = self.extras.timer.take() {
            ctx.every_with(Duration::from_secs(3600), move || {
                let _owned = &owned;
                Msg::Tick
            })
            .await
            .expect("the timer registers");
        }
        if let Some(owned) = self.extras.factory.take() {
            let owned = Mutex::new(owned);
            ctx.spawn_child(move || {
                let _owned = &owned;
                Idle
            })
            .await
            .expect("the child spawns");
        }
    }
}

impl Actor for Subject {
    type Message = Msg;
    type Response = ();

    async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(go) = self.extras.go.take() {
            let _ = go.await;
        }
        if self.start == Start::FailInit {
            self.attach(ctx).await;
            return Err(ActorError::user("init refused"));
        }
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.attach(ctx).await;
        if let Some(started) = self.extras.started.take() {
            let _ = started.send(());
        }
        if self.start == Start::Park {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Hold(release, began) => {
                let _ = began.send(());
                let _ = release.await;
            }
            Msg::Panic => panic!("the handler panicked"),
            Msg::Queued(_) | Msg::Tick | Msg::Stream => {}
        }
        Ok(())
    }
}

/// Which owned items panic when dropped.
#[derive(Clone, Copy, Default)]
struct Panics {
    value: bool,
    message: bool,
    stream: bool,
    timer: bool,
    factory: bool,
}

struct Rig {
    handle: ActorHandle<Subject>,
    join: JoinHandle<StopReason>,
    status: StatusRx,
    subtree: SubtreeWatch,
    system: Arc<ActorSystem>,
    name: String,
    probe: Arc<OnceLock<Probe>>,
    log: Log,
    panics: Panics,
    started: Option<oneshot::Receiver<()>>,
}

impl Rig {
    fn owned(&self, who: &'static str, panics: bool) -> Owned {
        Owned {
            who,
            probe: self.probe.clone(),
            log: self.log.clone(),
            panics,
            hold: None,
        }
    }

    /// Queues an owned message on each lane behind a held turn and returns
    /// its release.
    async fn queue_behind_a_held_turn(&self) -> oneshot::Sender<()> {
        let (release, held) = oneshot::channel();
        let (began_tx, began) = oneshot::channel();
        self.handle
            .notify(Msg::Hold(held, began_tx))
            .await
            .expect("the hold is queued");
        // Nothing else is queued until the turn is held, so no message can
        // overtake it.
        began.await.expect("the turn is held");
        self.handle
            .notify(Msg::Queued(self.owned("message", self.panics.message)))
            .await
            .expect("the message is queued");
        self.handle
            .notify_priority(Msg::Queued(self.owned("priority", self.panics.message)))
            .await
            .expect("the priority message is queued");
        release
    }

    /// Waits until the status plane reads as ended and the subtree is gone.
    /// The bound only limits how long a real hang takes to report.
    async fn wait_ended(&self) {
        let ended = async {
            let mut status = self.status.clone();
            while status.borrow().status != ActorStatus::Stopped {
                if status.changed().await.is_err() {
                    break;
                }
                // A change that is already pending resolves at once; yielding
                // lets the timeout around this wait fire if the status never
                // reaches Stopped.
                tokio::task::yield_now().await;
            }
            let mut wait = crate::actor::subtree::SubtreeWait::new(&self.subtree);
            while !self.subtree.is_gone() {
                tokio::time::timeout(Duration::from_secs(60), wait.changed())
                    .await
                    .expect("the subtree wait resolves");
            }
        };
        tokio::time::timeout(Duration::from_secs(60), ended)
            .await
            .expect("the actor ends");
    }

    /// Checks every recorded drop against the order of an actor's end that
    /// returned: the name is freed before its queued messages, its value and
    /// its context drop; its timers and streams are released from the
    /// mailbox's close on, before or after the name is freed.
    fn assert_order(&self, expected: &[&str], name_free_first: bool) {
        self.assert_order_on(expected, name_free_first, true);
    }

    /// [`assert_order`](Self::assert_order) for an abort before the
    /// mailbox's close: while suspended the name is freed before anything
    /// the actor owned drops, timers and streams included; before the first
    /// poll the value and the context drop before the registry guard
    /// (`name_free_first` false). After the close, timers and streams follow
    /// the return path's rule, and [`assert_order`](Self::assert_order)
    /// applies.
    fn assert_order_on_abort(&self, expected: &[&str], name_free_first: bool) {
        self.assert_order_on(expected, name_free_first, false);
    }

    /// [`assert_order`](Self::assert_order) for a task whose own poll
    /// unwound after the mailbox's close: the abort guard writes the reason
    /// once the name is free and before any queued message drops, and the
    /// timers and streams follow the return path's rule.
    fn assert_order_on_unwind(&self, expected: &[&str]) {
        self.assert_reason_written_once_the_name_is_free();
        self.assert_order(expected, true);
    }

    /// The abort guard wrote the reason once the name was free and before
    /// any queued message dropped.
    fn assert_reason_written_once_the_name_is_free(&self) {
        let seen = self.log.seen();
        let reason = seen
            .iter()
            .position(|s| s.who == "reason")
            .unwrap_or_else(|| panic!("the abort guard wrote no reason: {seen:?}"));
        assert!(
            !seen[reason].name_registered,
            "the reason was written while the name was registered: {seen:?}"
        );
        for (at, s) in seen.iter().enumerate() {
            if s.who == "message" || s.who == "priority" {
                assert!(
                    reason < at,
                    "{} dropped before the reason was written: {seen:?}",
                    s.who
                );
            }
        }
    }

    fn assert_order_on(&self, expected: &[&str], name_free_first: bool, returned: bool) {
        let seen = self.log.seen();
        // Aborted while suspended: the abort guard writes the reason once
        // the name is free and before any queued message drops.
        if !returned && name_free_first {
            self.assert_reason_written_once_the_name_is_free();
        }
        for who in expected {
            assert!(
                seen.iter().any(|s| s.who == *who),
                "{who} was not dropped before the end was read: {seen:?}"
            );
        }
        for s in &seen {
            assert!(
                !s.plane_ended,
                "the status plane read as ended before {} was dropped: {seen:?}",
                s.who
            );
            assert!(
                !s.subtree_gone,
                "the subtree read as ended before {} was dropped: {seen:?}",
                s.who
            );
            assert!(
                !s.hold_released,
                "the hold read as released before {} was dropped: {seen:?}",
                s.who
            );
            // A supervisor drops its child factories with its children's
            // teardown, whatever its stop reason, before its name is freed.
            if name_free_first && s.who == "factory" {
                assert!(
                    s.name_registered,
                    "the child factories drop before the name is freed: {seen:?}"
                );
            } else if returned && (s.who == "stream" || s.who == "timer") {
                // A timer or stream is released from the mailbox's close on,
                // when its forwarder next runs, or by the end's abort: before
                // or after the name is freed.
            } else if name_free_first && s.who != "name" {
                assert!(
                    !s.name_registered,
                    "the name was still registered when {} was dropped: {seen:?}",
                    s.who
                );
            }
        }
        assert!(
            self.system.get::<Subject>(&self.name).is_none(),
            "the name is free once the actor ended"
        );
    }
}

fn rig(start: Start, panics: Panics, with_ack: bool) -> Rig {
    rig_held(start, panics, with_ack, None)
}

/// [`rig`], with the actor value's drop held by `value_hold`.
fn rig_held(start: Start, panics: Panics, with_ack: bool, value_hold: Option<Hold>) -> Rig {
    let system = ActorSystem::create(format!("end-order-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("end-order-{}", uuid::Uuid::new_v4());
    let probe = Arc::new(OnceLock::new());
    let log = Log::default();
    let owned = |who, panics| Owned {
        who,
        probe: probe.clone(),
        log: log.clone(),
        panics,
        hold: None,
    };
    let (started_tx, started) = oneshot::channel();
    let (go_tx, go) = oneshot::channel();
    let mut value = owned("value", panics.value);
    value.hold = value_hold;
    let subject = Subject {
        _value: value,
        start,
        extras: Extras {
            stream: Some(owned("stream", panics.stream)),
            timer: Some(owned("timer", panics.timer)),
            factory: panics.factory.then(|| owned("factory", true)),
            started: Some(started_tx),
            go: Some(go),
        },
    };
    let mut config = ActorConfig::default().with_priority_lane(8);
    if panics.factory {
        config.supervision = Some(crate::actor::supervision::SupervisionConfig::default());
    }
    let (ack_tx, ack_rx) = if with_ack {
        let (tx, rx) = oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let (handle, join, subtree) = spawn_actor(
        ActorId::from(name.clone()),
        subject,
        config,
        Some(Arc::from(name.clone())),
        Some(system.clone()),
        ack_tx,
        true,
        // Far longer than any test waits: a `Kill` is read at a turn
        // boundary, and the tests that abort the task do so themselves, so
        // no host delay turns an orderly end into an abort.
        Duration::from_secs(60),
        None,
    )
    .expect("the actor spawns");
    let _ = probe.set(Probe {
        status: handle.status_rx(),
        subtree: subtree.clone(),
        system: system.clone(),
        name: name.clone(),
        ack: Mutex::new((ack_rx, None)),
    });
    // The abort guard's write of the reason, and the registry guard's release
    // of the name, are recorded among the drops as the items "reason" and
    // "name": what each saw of the name and the plane.
    for (point, who) in [
        (crate::actor::runtime::end_hooks::REASON, "reason"),
        (crate::actor::runtime::end_hooks::NAME, "name"),
    ] {
        let (log, system, name) = (log.clone(), system.clone(), name.clone());
        let (subtree, status) = (subtree.clone(), handle.status_rx());
        crate::actor::runtime::end_hooks::attach(point, ActorId::from(name.clone()), move || {
            log.0.lock().unwrap().push(Seen {
                who,
                plane_ended: status.has_changed().is_err()
                    || status.borrow().status == ActorStatus::Stopped,
                name_registered: system.get::<Subject>(&name).is_some(),
                subtree_gone: subtree.is_gone(),
                hold_released: subtree.is_released(),
                ack: AckRead::None,
            });
        });
    }
    let _ = go_tx.send(());
    Rig {
        status: handle.status_rx(),
        handle,
        join,
        subtree,
        system,
        name,
        probe,
        log,
        panics,
        started: Some(started),
    }
}

const ALL: &[&str] = &["message", "value", "stream", "timer"];
const ALL_QUEUED: &[&str] = &["message", "priority", "value", "stream", "timer"];

/// The actor returns from its run loop with `reason`; everything it owned is
/// dropped after its name is freed and before `Stopped`, which is published.
async fn a_returning_actor_ends_in_order(reason: StopReason) {
    let mut r = rig(Start::Run, Panics::default(), false);
    r.started.take().unwrap().await.expect("the actor started");
    let release = r.queue_behind_a_held_turn().await;
    r.handle.stop(reason.clone()).await.expect("accepted");
    let _ = release.send(());
    let returned = (&mut r.join).await.expect("the task returns");
    assert_eq!(
        std::mem::discriminant(&returned),
        std::mem::discriminant(&reason)
    );
    r.wait_ended().await;
    assert_eq!(
        r.status.borrow().status,
        ActorStatus::Stopped,
        "Stopped is published"
    );
    r.assert_order(ALL_QUEUED, true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_drops_everything_before_stopped_multi_thread() {
    a_returning_actor_ends_in_order(StopReason::Kill).await;
}

/// A task aborted while suspended drops its parameters last to first: the
/// registry guard before the queued messages, the context and the value,
/// and the status sender and the subtree hold last. The forwarders, aborted
/// with the context, hold the plane and the subtree until their own futures
/// are dropped.
async fn an_actor_aborted_while_suspended_ends_in_order() {
    let mut r = rig(Start::Park, Panics::default(), false);
    r.started
        .take()
        .unwrap()
        .await
        .expect("parked in on_started");
    r.handle
        .notify(Msg::Queued(r.owned("message", false)))
        .await
        .unwrap();
    r.join.abort();
    let joined = (&mut r.join).await;
    assert!(joined.unwrap_err().is_cancelled());
    r.wait_ended().await;
    assert_ne!(
        r.status.borrow().status,
        ActorStatus::Stopped,
        "an aborted task never publishes Stopped"
    );
    r.assert_order_on_abort(ALL, true);
}

#[tokio::test(flavor = "current_thread")]
async fn an_actor_aborted_while_suspended_drops_everything_first_current_thread() {
    an_actor_aborted_while_suspended_ends_in_order().await;
}

/// The task's own poll unwinds right after the registry guard freed the
/// name, in the tail of a graceful end: the rest of the end runs as an
/// abort's does. The reason, `Failure(Panic(..))` of an uncaught panic, is
/// written once the name is free and before any queued message drops,
/// everything the actor owned drops before the plane reads as ended, and
/// `Stopped` is never published.
async fn an_own_unwind_after_the_name_is_freed_ends_as_an_abort() {
    let mut r = rig(Start::Run, Panics::default(), false);
    r.started.take().unwrap().await.expect("the actor started");
    let release = r.queue_behind_a_held_turn().await;
    crate::actor::runtime::end_hooks::attach(
        crate::actor::runtime::end_hooks::NAME_FREED,
        ActorId::from(r.name.clone()),
        || panic!("the crate's own code panics once the name is freed"),
    );
    r.handle.stop(StopReason::Graceful).await.expect("accepted");
    let _ = release.send(());
    let joined = (&mut r.join).await;
    assert!(joined.unwrap_err().is_panic(), "the task's poll unwound");
    r.wait_ended().await;
    assert_ne!(
        r.status.borrow().status,
        ActorStatus::Stopped,
        "an unwound task never publishes Stopped"
    );
    assert_eq!(
        r.status.borrow().reason,
        Some(StopReason::Failure(ActorError::Panic(
            crate::actor::runtime::UNCAUGHT_PANIC.to_string()
        ))),
        "the plane reads the uncaught panic"
    );
    assert!(
        r.system.get::<Subject>(&r.name).is_none(),
        "the name stays free"
    );
    r.assert_order_on_unwind(ALL_QUEUED);
}

#[tokio::test(flavor = "current_thread")]
async fn an_own_unwind_after_the_name_is_freed_ends_as_an_abort_current_thread() {
    an_own_unwind_after_the_name_is_freed_ends_as_an_abort().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_own_unwind_after_the_name_is_freed_ends_as_an_abort_multi_thread() {
    an_own_unwind_after_the_name_is_freed_ends_as_an_abort().await;
}

/// A task aborted before its first poll drops its parameters first to last:
/// the value and the context, then the queued messages, then the registry
/// guard, which frees the name and only then releases the last status
/// sender and the last subtree hold. Current-thread only: the task must not
/// be polled before the abort, which only one thread can promise.
#[tokio::test(flavor = "current_thread")]
async fn an_actor_aborted_before_its_first_poll_drops_everything_first() {
    let mut r = rig(Start::Run, Panics::default(), false);
    r.handle
        .try_notify(Msg::Queued(r.owned("message", false)))
        .expect("the mailbox has room");
    r.join.abort();
    let joined = (&mut r.join).await;
    assert!(joined.unwrap_err().is_cancelled());
    let seen = r.log.seen();
    let message = r.log.of("message");
    assert!(
        message.name_registered,
        "the value and the queued messages drop before the registry guard here: {seen:?}"
    );
    // Nothing was polled: no stream or timer exists.
    r.assert_order_on_abort(&["value", "message"], false);
    assert!(
        r.status.has_changed().is_err(),
        "the plane is closed: every sender is gone"
    );
    assert!(r.subtree.is_gone());
}

/// A panic in a drop of the end is caught and `Stopped` is still
/// published. Before the reason is final (a registered stream's drop at the
/// forwarders' end, a child factory's with the teardown), the panic makes any
/// reason but an earlier `Kill` or `Failure` a failure. After it (a queued
/// message's or the value's drop, past the seal) the reason stands, on the
/// plane as in the task's return. With the `tracing` feature, a panic that
/// leaves the reason as it stands is logged once with the actor's id.
async fn a_panicking_drop_still_publishes_stopped(
    which: &str,
    panics: Panics,
    past_the_seal: bool,
) {
    #[cfg(feature = "tracing")]
    let logged = crate::actor::supervision::lost_runner::capture::install();
    for (request, kept) in [
        (StopReason::Graceful, false),
        (StopReason::Failure(ActorError::user("earlier")), true),
        (StopReason::Kill, true),
    ] {
        let mut r = rig(Start::Run, panics, false);
        r.started.take().unwrap().await.expect("the actor started");
        let release = r.queue_behind_a_held_turn().await;
        r.handle.stop(request.clone()).await.expect("accepted");
        let _ = release.send(());
        let returned = (&mut r.join).await.expect("the task returns its reason");
        match (&request, &returned) {
            (StopReason::Failure(a), StopReason::Failure(b)) => {
                assert_eq!(
                    a.to_string(),
                    b.to_string(),
                    "{which}: the earlier failure is kept"
                );
            }
            (StopReason::Kill, StopReason::Kill) => {}
            (StopReason::Graceful, StopReason::Graceful) => {
                assert!(past_the_seal, "{which}: {request:?} stayed {returned:?}");
            }
            (_, StopReason::Failure(ActorError::Panic(text))) => {
                assert!(
                    !kept && !past_the_seal,
                    "{which}: {request:?} became {returned:?}"
                );
                assert!(text.contains("panicked when dropped"), "{which}: {text}");
            }
            other => panic!("{which}: unexpected reasons {other:?}"),
        }
        r.wait_ended().await;
        assert_eq!(
            r.status.borrow().status,
            ActorStatus::Stopped,
            "{which}: Stopped is published after a panicking drop under {request:?}"
        );
        assert_eq!(
            r.status.borrow().reason.as_ref().map(ToString::to_string),
            Some(returned.to_string()),
            "{which}: the plane reads the reason the task returned"
        );
        #[cfg(feature = "tracing")]
        {
            let token = format!("actor_id={} ", r.name);
            let lines: Vec<String> = logged
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains(&token) && line.contains("the reason stands"))
                .cloned()
                .collect();
            // A panic that leaves the reason as it stands is logged once:
            // every panic past the seal, and one before it under an earlier
            // `Kill` or `Failure`. One that becomes the reason logs nothing.
            let want = usize::from(past_the_seal || kept);
            assert_eq!(lines.len(), want, "{which}: {want} warning(s): {lines:?}");
            let when = if past_the_seal { "after" } else { "before" };
            assert!(
                lines.iter().all(|line| line.contains(when)),
                "{which}: {lines:?}"
            );
        }
        r.assert_order(ALL_QUEUED, true);
    }
}

/// Two drops of one step of the end panic: the step keeps the first and
/// discards the second, and with the `tracing` feature the discarded one is
/// logged once, naming the actor. The queued message and the queued
/// priority message drop after the seal, the mailbox first; a stream and a
/// timer still registered drop at the forwarders' end, before it.
#[cfg(feature = "tracing")]
async fn a_discarded_panic_of_the_end_is_logged_once(panics: Panics, discarded: &str) {
    let logged = crate::actor::supervision::lost_runner::capture::install();
    let mut r = rig(Start::Run, panics, false);
    r.started.take().unwrap().await.expect("the actor started");
    let release = r.queue_behind_a_held_turn().await;
    r.handle.stop(StopReason::Graceful).await.expect("accepted");
    let _ = release.send(());
    let _ = (&mut r.join).await.expect("the task returns its reason");
    r.wait_ended().await;
    let token = format!("actor_id={} ", r.name);
    let lines: Vec<String> = logged
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(&token) && line.contains("this panic is discarded"))
        .cloned()
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "one discarded panic, logged once: {lines:?}"
    );
    assert!(lines[0].contains(discarded), "{lines:?}");
}

#[cfg(feature = "tracing")]
#[tokio::test(flavor = "current_thread")]
async fn a_discarded_queued_message_panic_is_logged_once() {
    let panics = Panics {
        message: true,
        ..Panics::default()
    };
    a_discarded_panic_of_the_end_is_logged_once(panics, "priority panicked when dropped").await;
}

#[cfg(feature = "tracing")]
#[tokio::test(flavor = "current_thread")]
async fn a_discarded_forwarder_panic_of_the_end_is_logged_once() {
    let panics = Panics {
        stream: true,
        timer: true,
        ..Panics::default()
    };
    a_discarded_panic_of_the_end_is_logged_once(panics, "panicked when dropped").await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_panicking_child_factory_drop_still_publishes_stopped() {
    let panics = Panics {
        factory: true,
        ..Panics::default()
    };
    a_panicking_drop_still_publishes_stopped("factory", panics, false).await;
}

/// A timer factory or stream that panics when dropped, recording at its
/// drop whether its actor's name was still registered: registered means
/// the drop ran during the run, freed means the end ran it.
struct CancelledDrop {
    system: Arc<ActorSystem>,
    name: String,
    seen: Arc<Mutex<Vec<bool>>>,
    dropped: Arc<tokio::sync::Semaphore>,
}

impl Drop for CancelledDrop {
    fn drop(&mut self) {
        let registered = self.system.get::<CancelsBoth>(&self.name).is_some();
        self.seen.lock().unwrap().push(registered);
        self.dropped.add_permits(1);
        panic!("a cancelled timer or stream panicked when dropped");
    }
}

struct PendingStream(#[allow(dead_code)] CancelledDrop);

impl Stream for PendingStream {
    type Item = ();
    fn poll_next(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        Poll::Pending
    }
}

enum CancelMsg {
    Tick,
    Stream,
    Cancel,
}

impl From<StreamEvent<()>> for CancelMsg {
    fn from(_: StreamEvent<()>) -> Self {
        CancelMsg::Stream
    }
}

/// Where the cancelled timer and stream are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelOrder {
    /// The handler cancels both and waits until both are dropped, then
    /// stops: their forwarders run before the end.
    RunFirst,
    /// The handler cancels both and stops without a yield of its own. The
    /// one yield the stop takes runs their cancelled forwarders: on a
    /// current_thread runtime both before the end; on the one-worker
    /// multi-thread runtime these tests use, the worker runs one of them in
    /// that yield and the end runs the other after the name is freed.
    NoWait,
    /// `on_stopped` cancels both, with no yield before the end.
    InOnStopped,
}

struct CancelsBoth {
    order: CancelOrder,
    timer: Option<CancelledDrop>,
    stream: Option<CancelledDrop>,
    ids: Option<(crate::types::RecurringId, crate::types::StreamId)>,
    dropped: Arc<tokio::sync::Semaphore>,
}

impl CancelsBoth {
    fn cancel(&mut self, ctx: &mut ActorContext<Self>) {
        let (timer, stream) = self.ids.take().expect("registered in on_started");
        ctx.cancel_timer(timer).expect("the timer is registered");
        ctx.cancel_stream(stream).expect("the stream is registered");
    }
}

impl Actor for CancelsBoth {
    type Message = CancelMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let guard = self.timer.take().expect("a timer guard");
        let timer = ctx
            .every_with(Duration::from_secs(3600), move || {
                let _ = &guard;
                CancelMsg::Tick
            })
            .await
            .expect("the timer registers");
        let stream = ctx.add_stream(PendingStream(self.stream.take().expect("a stream guard")));
        self.ids = Some((timer, stream));
        Ok(())
    }

    async fn handle(&mut self, msg: CancelMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let CancelMsg::Cancel = msg {
            match self.order {
                CancelOrder::RunFirst => {
                    self.cancel(ctx);
                    self.dropped
                        .acquire_many(2)
                        .await
                        .expect("the semaphore stays open")
                        .forget();
                }
                CancelOrder::NoWait => self.cancel(ctx),
                CancelOrder::InOnStopped => {}
            }
            ctx.self_handle()
                .stop(StopReason::Graceful)
                .await
                .expect("accepted");
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if self.order == CancelOrder::InOnStopped {
            self.cancel(ctx);
        }
        Ok(())
    }
}

/// A timer or stream cancelled during the run (`on_stopped` included)
/// belongs to the run: a panic in its drop never changes the end, whether
/// its forwarder runs before the end or the end aborts it first. Each order
/// is confirmed by what the drops saw: the name still registered when the
/// forwarders ran first, already freed when the end ran them. The orders
/// asserted are the ones a current_thread runtime and a one-worker
/// multi-thread runtime give; with more workers another worker can take a
/// cancelled forwarder at any point.
async fn a_cancelled_forwarder_s_drop_panic_never_changes_the_end(
    order: CancelOrder,
    one_thread: bool,
) {
    let system = ActorSystem::create(format!("end-order-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("end-order-{}", uuid::Uuid::new_v4());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let dropped = Arc::new(tokio::sync::Semaphore::new(0));
    let guard = || CancelledDrop {
        system: system.clone(),
        name: name.clone(),
        seen: seen.clone(),
        dropped: dropped.clone(),
    };
    let (handle, join, _subtree) = spawn_actor(
        ActorId::from(name.clone()),
        CancelsBoth {
            order,
            timer: Some(guard()),
            stream: Some(guard()),
            ids: None,
            dropped: dropped.clone(),
        },
        ActorConfig::default(),
        Some(Arc::from(name.clone())),
        Some(system.clone()),
        None,
        true,
        Duration::from_millis(200),
        None,
    )
    .expect("the actor spawns");
    handle.try_notify(CancelMsg::Cancel).unwrap();
    let returned = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("the actor ends")
        .expect("the task returns its reason");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{order:?}: both guards were dropped");
    match order {
        CancelOrder::RunFirst => assert_eq!(seen, vec![true; 2], "{order:?}"),
        CancelOrder::InOnStopped => assert_eq!(seen, vec![false; 2], "{order:?}"),
        CancelOrder::NoWait if one_thread => assert_eq!(seen, vec![true; 2], "{order:?}"),
        CancelOrder::NoWait => assert_eq!(seen, vec![true, false], "{order:?}"),
    }
    assert!(
        matches!(returned, StopReason::Graceful),
        "{order:?}: ended as {returned:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_cancelled_forwarder_s_drop_panic_never_changes_the_end_multi_thread() {
    for order in [
        CancelOrder::RunFirst,
        CancelOrder::NoWait,
        CancelOrder::InOnStopped,
    ] {
        a_cancelled_forwarder_s_drop_panic_never_changes_the_end(order, false).await;
    }
}

/// A registered stream whose drop panics, yielding items as fast as the
/// actor's one-slot mailbox takes them, and counting them.
struct Flooding {
    _bomb: FloodBomb,
    yielded: Arc<std::sync::atomic::AtomicUsize>,
}

struct FloodBomb(Arc<Mutex<Vec<&'static str>>>);

impl Drop for FloodBomb {
    fn drop(&mut self) {
        self.0.lock().unwrap().push("dropped");
        panic!("a registered stream panicked when dropped");
    }
}

impl Stream for Flooding {
    type Item = ();
    fn poll_next(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        self.yielded
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Poll::Ready(Some(()))
    }
}

struct FloodMsg;

impl From<StreamEvent<()>> for FloodMsg {
    fn from(_: StreamEvent<()>) -> Self {
        FloodMsg
    }
}

struct IdleChild;

impl Actor for IdleChild {
    type Message = ();
    type Response = ();
    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A supervisor with a one-slot mailbox and a flooding stream. With a
/// child, its teardown yields between the mailbox's close and the end's
/// abort of the forwarders; without one, it does not.
struct FloodedSupervisor {
    stream: Option<Flooding>,
    yielded: Arc<std::sync::atomic::AtomicUsize>,
    handled: usize,
    with_child: bool,
}

impl Actor for FloodedSupervisor {
    type Message = FloodMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.add_stream(self.stream.take().expect("the stream"));
        if self.with_child {
            ctx.spawn_child(|| IdleChild)
                .await
                .expect("the child spawns");
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: FloodMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        // The forwarder fills the one slot behind this message and then
        // waits in its send with one more: two items past this one.
        self.handled += 1;
        while self.yielded.load(std::sync::atomic::Ordering::SeqCst) < self.handled + 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        ctx.self_handle()
            .stop(StopReason::Graceful)
            .await
            .expect("accepted");
        Ok(())
    }
}

async fn a_stream_parked_on_the_full_mailbox_ends(with_child: bool) -> StopReason {
    let system = ActorSystem::create(format!("end-order-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("end-order-{}", uuid::Uuid::new_v4());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let yielded = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut config = ActorConfig::default().with_mailbox_capacity(1);
    config.supervision = Some(crate::actor::supervision::SupervisionConfig::default());
    let (_handle, join, _subtree) = spawn_actor(
        ActorId::from(name.clone()),
        FloodedSupervisor {
            stream: Some(Flooding {
                _bomb: FloodBomb(seen.clone()),
                yielded: yielded.clone(),
            }),
            yielded,
            handled: 0,
            with_child,
        },
        config,
        Some(Arc::from(name.clone())),
        Some(system),
        None,
        true,
        Duration::from_millis(200),
        None,
    )
    .expect("the actor spawns");
    let reason = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("the actor ends")
        .expect("the task returns its reason");
    assert_eq!(*seen.lock().unwrap(), vec!["dropped"]);
    reason
}

/// A registered stream waiting in its send when the mailbox closes is woken
/// by the close. Whether its forwarder runs before the end's abort (the
/// teardown of a supervisor with a child yields) and releases it there, or
/// the abort drops it (no child), its drop's panic is classified by its
/// registration, which is live: the end's, the same reason either way.
async fn a_stream_parked_on_the_full_mailbox_ends_the_same_either_way() {
    let with = a_stream_parked_on_the_full_mailbox_ends(true).await;
    let without = a_stream_parked_on_the_full_mailbox_ends(false).await;
    assert!(
        matches!(&with, StopReason::Failure(ActorError::Panic(_))),
        "with a child: {with:?}"
    );
    assert!(
        matches!(&without, StopReason::Failure(ActorError::Panic(_))),
        "without a child: {without:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_parked_on_the_full_mailbox_ends_the_same_either_way_multi_thread() {
    a_stream_parked_on_the_full_mailbox_ends_the_same_either_way().await;
}

/// The parent's stream, fed by its child through a one-slot channel.
struct ChannelStream(tokio::sync::mpsc::Receiver<u32>);

impl Stream for ChannelStream {
    type Item = u32;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<u32>> {
        self.0.poll_recv(cx)
    }
}

struct Fed;

impl From<StreamEvent<u32>> for Fed {
    fn from(_: StreamEvent<u32>) -> Self {
        Fed
    }
}

/// Sends into its parent's stream until the stream is gone, in one turn.
struct Producer {
    tx: tokio::sync::mpsc::Sender<u32>,
    stopped: Arc<Mutex<Option<StopReason>>>,
}

impl Actor for Producer {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.self_handle()
            .notify(())
            .await
            .expect("the producer's turn");
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let mut n = 0;
        while self.tx.send(n).await.is_ok() {
            n += 1;
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.stopped.lock().unwrap() = Some(reason.clone());
        Ok(())
    }
}

/// A supervisor with a one-slot mailbox whose stream its producer child
/// feeds. It stops itself once its mailbox and the channel are both full,
/// so the producer waits in its send and the stream's forwarder in its own.
struct Consumer {
    rx: Option<tokio::sync::mpsc::Receiver<u32>>,
    tx: tokio::sync::mpsc::Sender<u32>,
    stopped: Arc<Mutex<Option<StopReason>>>,
    shutdown: crate::types::Shutdown,
}

impl Actor for Consumer {
    type Message = Fed;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.add_stream(ChannelStream(self.rx.take().expect("the receiver")));
        let (tx, stopped) = (self.tx.clone(), self.stopped.clone());
        ctx.spawn_child(move || Producer {
            tx: tx.clone(),
            stopped: stopped.clone(),
        })
        .shutdown(self.shutdown)
        .await
        .expect("the child spawns");
        Ok(())
    }

    async fn handle(&mut self, _msg: Fed, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let me = ctx.self_handle().clone();
        while !(me.mailbox_len() == 1 && self.tx.capacity() == 0) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        me.stop(StopReason::Graceful).await.expect("accepted");
        Ok(())
    }
}

/// The parent stops while its forwarder waits to deliver the child's next
/// item. The mailbox's close wakes the forwarder, which releases the stream
/// at once: the child's send fails, its turn ends, and it stops at the
/// parent's request, inside its window and also under `Infinity`.
async fn a_producer_child_stops_at_its_parent_s_request(shutdown: crate::types::Shutdown) {
    let system = ActorSystem::create(format!("end-order-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("end-order-{}", uuid::Uuid::new_v4());
    let stopped = Arc::new(Mutex::new(None));
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let mut config = ActorConfig::default().with_mailbox_capacity(1);
    config.supervision = Some(crate::actor::supervision::SupervisionConfig::default());
    let (_handle, join, _subtree) = spawn_actor(
        ActorId::from(name.clone()),
        Consumer {
            rx: Some(rx),
            tx,
            stopped: stopped.clone(),
            shutdown,
        },
        config,
        Some(Arc::from(name)),
        Some(system),
        None,
        true,
        Duration::from_millis(200),
        None,
    )
    .expect("the actor spawns");
    let reason = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("the parent ends")
        .expect("the task returns its reason");
    assert!(matches!(reason, StopReason::Graceful), "{reason:?}");
    assert!(
        matches!(*stopped.lock().unwrap(), Some(StopReason::ParentRequest)),
        "the child ran on_stopped with the parent's request: {:?}",
        stopped.lock().unwrap()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_producer_child_stops_at_its_parent_s_request_current_thread() {
    a_producer_child_stops_at_its_parent_s_request(crate::types::Shutdown::Timeout(
        Duration::from_secs(5),
    ))
    .await;
}

/// What a forwarder owns that a child of the actor waits on.
#[derive(Clone, Copy, Debug)]
enum Held {
    IdleStream,
    Factory,
    OneShot,
}

enum HeldMsg {
    Fed,
    #[allow(dead_code)]
    Hold(Option<tokio::sync::mpsc::Sender<()>>),
}

impl From<StreamEvent<u32>> for HeldMsg {
    fn from(_: StreamEvent<u32>) -> Self {
        HeldMsg::Fed
    }
}

enum WaitOn {
    TxClosed(tokio::sync::mpsc::Sender<u32>),
    RxDrained(tokio::sync::mpsc::Receiver<()>),
}

struct Waiter {
    wait: Option<WaitOn>,
    entered: Arc<std::sync::atomic::AtomicBool>,
    stopped: Arc<Mutex<Option<StopReason>>>,
}

impl Actor for Waiter {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.self_handle().notify(()).await.expect("turn");
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        match self.wait.take() {
            Some(WaitOn::TxClosed(tx)) => tx.closed().await,
            Some(WaitOn::RxDrained(mut rx)) => while rx.recv().await.is_some() {},
            None => {}
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.stopped.lock().unwrap() = Some(reason.clone());
        Ok(())
    }
}

struct HoldingParent {
    hold: Held,
    entered: Arc<std::sync::atomic::AtomicBool>,
    stopped: Arc<Mutex<Option<StopReason>>>,
    shutdown: crate::types::Shutdown,
}

impl Actor for HoldingParent {
    type Message = HeldMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let wait = match self.hold {
            Held::IdleStream => {
                let (tx, rx) = tokio::sync::mpsc::channel::<u32>(1);
                ctx.add_stream(ChannelStream(rx));
                WaitOn::TxClosed(tx)
            }
            Held::Factory => {
                let (tx, rx) = tokio::sync::mpsc::channel::<()>(1);
                ctx.every_with(Duration::from_secs(3600), move || {
                    HeldMsg::Hold(Some(tx.clone()))
                })
                .await
                .expect("timer");
                WaitOn::RxDrained(rx)
            }
            Held::OneShot => {
                let (tx, rx) = tokio::sync::mpsc::channel::<()>(1);
                ctx.schedule(HeldMsg::Hold(Some(tx)))
                    .after(Duration::from_secs(3600))
                    .await
                    .expect("timer");
                WaitOn::RxDrained(rx)
            }
        };
        let cell = Arc::new(Mutex::new(Some(wait)));
        let (entered, stopped) = (self.entered.clone(), self.stopped.clone());
        ctx.spawn_child(move || Waiter {
            wait: cell.lock().unwrap().take(),
            entered: entered.clone(),
            stopped: stopped.clone(),
        })
        .shutdown(self.shutdown)
        .await
        .expect("the child spawns");
        Ok(())
    }

    async fn handle(&mut self, _msg: HeldMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Stops the parent once its child waits in its turn; returns whether the
/// parent ended within 5 s, the child's `on_stopped` reason, and the time.
async fn a_child_waiting_on_a_forwarder_s_release(
    hold: Held,
    shutdown: crate::types::Shutdown,
) -> (bool, Option<StopReason>, Duration) {
    let system = ActorSystem::create(format!("end-order-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("end-order-{}", uuid::Uuid::new_v4());
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = Arc::new(Mutex::new(None));
    let config = ActorConfig {
        supervision: Some(crate::actor::supervision::SupervisionConfig::default()),
        ..ActorConfig::default()
    };
    let (handle, join, _subtree) = spawn_actor(
        ActorId::from(name.clone()),
        HoldingParent {
            hold,
            entered: entered.clone(),
            stopped: stopped.clone(),
            shutdown,
        },
        config,
        Some(Arc::from(name)),
        Some(system),
        None,
        true,
        Duration::from_millis(200),
        None,
    )
    .expect("the actor spawns");
    while !entered.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let t0 = std::time::Instant::now();
    handle.stop(StopReason::Graceful).await.expect("accepted");
    let ended = tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .is_ok();
    let r = stopped.lock().unwrap().clone();
    (ended, r, t0.elapsed())
}

/// A forwarder the close does not wake on its own (a stream with nothing to
/// yield, a recurring timer's factory between ticks, a one-shot's message
/// before it fires) still releases what it owns when the mailbox closes,
/// so a child waiting on that release finishes its turn and stops at its
/// parent's request.
async fn every_forwarder_releases_at_the_close(shutdown: crate::types::Shutdown) {
    for hold in [Held::IdleStream, Held::Factory, Held::OneShot] {
        let (ended, child, _) = a_child_waiting_on_a_forwarder_s_release(hold, shutdown).await;
        assert!(ended, "{hold:?}: the parent ends");
        assert!(
            matches!(child, Some(StopReason::ParentRequest)),
            "{hold:?}: the child stops at its parent's request: {child:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_forwarder_releases_at_the_close_multi_thread() {
    every_forwarder_releases_at_the_close(crate::types::Shutdown::Timeout(Duration::from_millis(
        500,
    )))
    .await;
}

/// A stream whose one poll holds its worker thread until the test releases it
/// (or drops the release sender), and which then ends.
struct StraddlingPoll {
    _bomb: FloodBomb,
    entered: Option<oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}

impl Stream for StraddlingPoll {
    type Item = ();

    fn poll_next(mut self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        // The bound only limits how long a real hang holds the worker.
        let _ = self.release.recv_timeout(Duration::from_secs(60));
        Poll::Ready(None)
    }
}

/// Cancels its stream in `on_stopped`.
struct CancelsInOnStopped {
    stream: Option<StraddlingPoll>,
}

impl Actor for CancelsInOnStopped {
    type Message = FloodMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.add_stream(self.stream.take().expect("the stream"));
        Ok(())
    }

    async fn handle(&mut self, _msg: FloodMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _r: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        ctx.cancel_all_streams();
        Ok(())
    }
}

/// A stream cancelled in `on_stopped` while one of its polls is running, and
/// whose poll returns only after the mailbox closed, belongs to the run: the
/// forwarder drops it at the close, and the end reads that drop by the
/// registration, so its panic never changes the actor's reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_cancelled_in_on_stopped_whose_poll_ends_after_the_close_is_the_run_s() {
    let system = ActorSystem::create(format!("end-order-{}", uuid::Uuid::new_v4())).unwrap();
    let name = format!("end-order-{}", uuid::Uuid::new_v4());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (entered_tx, entered) = oneshot::channel();
    let (release_tx, release) = std::sync::mpsc::channel();
    let (handle, join, _subtree) = spawn_actor(
        ActorId::from(name.clone()),
        CancelsInOnStopped {
            stream: Some(StraddlingPoll {
                _bomb: FloodBomb(seen.clone()),
                entered: Some(entered_tx),
                release,
            }),
        },
        ActorConfig::default(),
        Some(Arc::from(name)),
        Some(system),
        None,
        true,
        Duration::from_millis(200),
        None,
    )
    .expect("the actor spawns");
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .expect("the stream is polled")
        .unwrap();
    handle.stop(StopReason::Graceful).await.expect("accepted");
    tokio::time::timeout(Duration::from_secs(10), handle.mailbox_close())
        .await
        .expect("the mailbox closes");
    release_tx.send(()).expect("the poll is still waiting");
    let reason = tokio::time::timeout(Duration::from_secs(10), join)
        .await
        .expect("the actor ends")
        .expect("the task returns its reason");
    assert_eq!(*seen.lock().unwrap(), vec!["dropped"]);
    assert!(matches!(reason, StopReason::Graceful), "{reason:?}");
}
