#![allow(dead_code)]
//! One condition, one outcome: a hook's future that completes and then panics
//! when it is dropped ends the actor exactly as a panic in the body of the hook
//! does, at every hook the run loop calls, whether the hook completed with its
//! normal output or with an error. The panic is the hook's result: no reply,
//! start, veto or cleanup error that the hook produced stands, and the actor is
//! handed no further message.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, AskError, ChildEvent, RestartType, SpawnError,
    StopReason, SupervisionAction,
};

mod common;

/// The text of every panic below.
const BOOM: &str = "a hook's future panicked";

/// How long a scenario waits for the actor to end.
const BOUND: Duration = Duration::from_secs(5);

/// What goes wrong in the hook under test.
#[derive(Clone, Copy, Debug)]
enum How {
    /// The body of the hook panics.
    PanicInBody,
    /// The hook completes with its normal output, then the drop of its future
    /// panics.
    PanicInDrop,
    /// The hook completes with an error (a veto, for `pre_stop`), then the
    /// drop of its future panics.
    ErrThenPanicInDrop,
}

const ALL: [How; 3] = [How::PanicInBody, How::PanicInDrop, How::ErrThenPanicInDrop];

/// A hook's future, written by hand: ready at its first poll, and panicking
/// either in that poll or when it is dropped afterwards.
struct Scripted<T> {
    output: Option<T>,
    panic_in_poll: bool,
    panic_on_drop: bool,
}

impl<T> Scripted<T> {
    /// A hook that does nothing wrong.
    fn fine(output: T) -> Self {
        Self {
            output: Some(output),
            panic_in_poll: false,
            panic_on_drop: false,
        }
    }

    /// The hook under test: it returns `normal`, or `other` when `how` is
    /// `ErrThenPanicInDrop`, and panics as `how` says.
    fn under_test(how: How, normal: T, other: T) -> Self {
        match how {
            How::PanicInBody => Self {
                output: Some(normal),
                panic_in_poll: true,
                panic_on_drop: false,
            },
            How::PanicInDrop => Self {
                output: Some(normal),
                panic_in_poll: false,
                panic_on_drop: true,
            },
            How::ErrThenPanicInDrop => Self {
                output: Some(other),
                panic_in_poll: false,
                panic_on_drop: true,
            },
        }
    }
}

impl<T: Unpin> Future for Scripted<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        if this.panic_in_poll {
            panic!("{BOOM}");
        }
        Poll::Ready(this.output.take().expect("polled once"))
    }
}

impl<T> Drop for Scripted<T> {
    fn drop(&mut self) {
        if self.panic_on_drop && !std::thread::panicking() {
            panic!("{BOOM}");
        }
    }
}

/// What the actors record about their own hooks.
#[derive(Default)]
struct Record {
    handled: AtomicU32,
    stopped: AtomicU32,
    saw: Mutex<Option<StopReason>>,
}

impl Record {
    fn on_stopped(&self, reason: &StopReason) {
        self.stopped.fetch_add(1, Ordering::SeqCst);
        *self.saw.lock().unwrap() = Some(reason.clone());
    }

    fn outcome(&self, caller: String, reason: Option<StopReason>) -> Outcome {
        Outcome {
            caller,
            reason,
            on_stopped: (
                self.stopped.load(Ordering::SeqCst),
                self.saw.lock().unwrap().clone(),
            ),
            handled: self.handled.load(Ordering::SeqCst),
        }
    }
}

/// What a scenario observed.
#[derive(Debug, PartialEq)]
struct Outcome {
    /// What the caller that triggered the hook read.
    caller: String,
    /// The reason the actor ended with; `None` when it never started or did
    /// not end in time.
    reason: Option<StopReason>,
    /// How many times `on_stopped` ran, and the reason it received.
    on_stopped: (u32, Option<StopReason>),
    /// How many messages `handle` was handed.
    handled: u32,
}

/// The hook under test.
#[derive(Clone, Copy, Debug)]
enum Site {
    PreStart,
    OnStarted,
    HandleSend,
    HandleNotify,
    PreStop,
    OnStopped,
    ChildStopped,
    FinalDelivery,
}

fn panicked() -> StopReason {
    StopReason::Failure(ActorError::Panic(BOOM.to_string()))
}

/// The outcome of a panic in the body of the hook, which a panic in the drop
/// of its completed future must have too.
fn expected(site: Site) -> Outcome {
    let failed = |caller: String, saw: StopReason, handled: u32| Outcome {
        caller,
        reason: Some(panicked()),
        on_stopped: (1, Some(saw)),
        handled,
    };
    let notified = "notify: Ok(())".to_string();
    match site {
        Site::PreStart | Site::OnStarted => Outcome {
            caller: format!(
                "spawn: {:?}",
                SpawnError::Init(Box::new(ActorError::Panic(BOOM.to_string())))
            ),
            reason: None,
            on_stopped: (0, None),
            handled: 0,
        },
        Site::HandleSend => failed(
            format!(
                "send: {:?}",
                Err::<u32, AskError>(AskError::Actor(ActorError::Panic(BOOM.to_string())))
            ),
            panicked(),
            1,
        ),
        Site::HandleNotify => failed(notified, panicked(), 1),
        Site::PreStop => failed("stop: Ok(())".to_string(), panicked(), 0),
        // `on_stopped` received the reason of the stop; its panic is the
        // reason the actor ends with.
        Site::OnStopped => failed("stop: Ok(())".to_string(), StopReason::Graceful, 0),
        Site::ChildStopped => failed(notified, panicked(), 0),
        // The delivery is part of the stop: `on_stopped` saw the stop's own
        // reason, and the failed delivery is the reason the actor ends with.
        Site::FinalDelivery => failed(notified, StopReason::Graceful, 0),
    }
}

// ---------------------------------------------------------------------------
// pre_start, on_started, handle, pre_stop and on_stopped of a plain actor
// ---------------------------------------------------------------------------

struct Plain {
    site: Site,
    how: How,
    record: Arc<Record>,
}

impl Plain {
    /// The hook's future: the one under test when `here`, otherwise a hook
    /// that does nothing wrong.
    fn hook<T>(&self, here: bool, normal: T, other: T) -> Scripted<T> {
        if here {
            Scripted::under_test(self.how, normal, other)
        } else {
            Scripted::fine(normal)
        }
    }
}

impl Actor for Plain {
    type Message = u32;
    type Response = u32;

    fn pre_start(
        &mut self,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        self.hook(
            matches!(self.site, Site::PreStart),
            Ok(()),
            Err(ActorError::user("pre_start failed")),
        )
    }

    fn on_started(
        &mut self,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        self.hook(
            matches!(self.site, Site::OnStarted),
            Ok(()),
            Err(ActorError::user("on_started failed")),
        )
    }

    fn handle(
        &mut self,
        msg: u32,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<u32>> + Send {
        self.record.handled.fetch_add(1, Ordering::SeqCst);
        self.hook(
            matches!(self.site, Site::HandleSend | Site::HandleNotify),
            Ok(msg + 1),
            Err(ActorError::user("handle failed")),
        )
    }

    fn pre_stop(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = bool> + Send {
        // `true` accepts the stop, `false` vetoes it.
        self.hook(matches!(self.site, Site::PreStop), true, false)
    }

    fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        self.record.on_stopped(reason);
        self.hook(
            matches!(self.site, Site::OnStopped),
            Ok(()),
            Err(ActorError::user("on_stopped failed")),
        )
    }
}

/// Waits for the actor to end.
async fn wait_ended<A: Actor>(handle: &ActorHandle<A>) -> Option<StopReason> {
    tokio::time::timeout(BOUND, handle.wait_stopped())
        .await
        .ok()
}

/// Spawns a [`Plain`] actor whose hook at `site` goes wrong as `how` says,
/// drives it to that hook, and reports what happened.
async fn plain(site: Site, how: How) -> Outcome {
    let record = Arc::new(Record::default());
    let actor = Plain {
        site,
        how,
        record: record.clone(),
    };
    let handle = match actor.spawn().await {
        Ok(handle) => handle,
        Err(err) => return record.outcome(format!("spawn: {err:?}"), None),
    };
    let caller = match site {
        Site::PreStart | Site::OnStarted => {
            // The start was not refused: nothing ends the actor, so there is
            // nothing to wait for.
            return record.outcome("spawn: started".to_string(), None);
        }
        Site::HandleSend => {
            let first = format!("send: {:?}", handle.send(1).await);
            // The actor is handed no further message.
            let _ = handle.send(2).await;
            first
        }
        Site::HandleNotify => {
            let first = format!("notify: {:?}", handle.notify(1).await);
            let _ = handle.notify(2).await;
            first
        }
        Site::PreStop | Site::OnStopped => {
            format!("stop: {:?}", handle.stop(StopReason::Graceful).await)
        }
        Site::ChildStopped | Site::FinalDelivery => unreachable!("a supervisor's hook"),
    };
    let reason = wait_ended(&handle).await;
    record.outcome(caller, reason)
}

fn check_plain(site: Site) {
    common::run(2, async move {
        for how in ALL {
            assert_eq!(plain(site, how).await, expected(site), "{site:?} {how:?}");
        }
    });
}

#[test]
fn a_completed_handle_future_whose_drop_panics_fails_the_send_as_a_panic_in_handle_does() {
    check_plain(Site::HandleSend);
}

// ---------------------------------------------------------------------------
// on_child_stopped of a supervisor, from its loop and from its final deliveries
// ---------------------------------------------------------------------------

/// A child that fails the first message it is sent.
struct Crasher;

impl Actor for Crasher {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("child crashed"))
    }
}

enum Order {
    /// Starts a child that fails at once.
    ChildFails,
    /// Starts a child and stops it, then asks for the supervisor's own stop
    /// before its loop reads the child's death, so the death is left for the
    /// final deliveries.
    StopWithAPendingDeath,
}

struct Watcher {
    final_delivery: bool,
    how: How,
    record: Arc<Record>,
}

impl Actor for Watcher {
    type Message = Order;
    type Response = ();

    async fn handle(&mut self, msg: Order, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let child = ctx
            .spawn_child(|| Crasher)
            .restart_type(RestartType::Temporary)
            .await?;
        match msg {
            Order::ChildFails => {
                child.notify(()).await.ok();
            }
            Order::StopWithAPendingDeath => {
                child.stop(StopReason::Cancelled).await.unwrap();
                child.wait_stopped().await;
                ctx.self_handle().stop(StopReason::Graceful).await.unwrap();
            }
        }
        Ok(())
    }

    fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        let in_final_delivery = event.action == SupervisionAction::SupervisorStopping;
        if in_final_delivery == self.final_delivery {
            Scripted::under_test(self.how, Ok(()), Err(ActorError::user("delivery failed")))
        } else {
            Scripted::fine(Ok(()))
        }
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.record.on_stopped(reason);
        Ok(())
    }
}

/// Spawns a [`Watcher`] whose `on_child_stopped` goes wrong as `how` says in
/// the delivery from its loop, or in a final delivery, and reports what
/// happened.
async fn supervisor(final_delivery: bool, how: How) -> Outcome {
    let record = Arc::new(Record::default());
    let handle = Watcher {
        final_delivery,
        how,
        record: record.clone(),
    }
    .spawn()
    .supervisor()
    .await
    .unwrap();
    let order = if final_delivery {
        Order::StopWithAPendingDeath
    } else {
        Order::ChildFails
    };
    let caller = format!("notify: {:?}", handle.notify(order).await);
    let reason = wait_ended(&handle).await;
    record.outcome(caller, reason)
}

fn check_supervisor(site: Site, final_delivery: bool) {
    common::run(2, async move {
        for how in ALL {
            assert_eq!(
                supervisor(final_delivery, how).await,
                expected(site),
                "{site:?} {how:?}"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// The same outcome from a handler written as an `async fn`
// ---------------------------------------------------------------------------

/// A value that panics when it is dropped.
struct PanicsWhenDropped;

impl Drop for PanicsWhenDropped {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            panic!("{BOOM}");
        }
    }
}

/// A handler whose last poll drops a local that panics.
struct LocalDrop {
    record: Arc<Record>,
}

impl Actor for LocalDrop {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, msg: u32, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        self.record.handled.fetch_add(1, Ordering::SeqCst);
        let _local = PanicsWhenDropped;
        Ok(msg + 1)
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.record.on_stopped(reason);
        Ok(())
    }
}
