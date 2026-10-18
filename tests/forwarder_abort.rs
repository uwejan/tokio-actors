//! The panics of a timer's or stream's task at its actor's end. A
//! forwarder parked on a full mailbox owns two user values: a recurring
//! timer's tick and its factory, or a stream's item and the stream. When
//! both drops panic, the process does not abort, the actor's reason is the
//! first panic, and with the `tracing` feature the second is logged once,
//! naming the actor. A factory or a stream whose drop panics alone is the
//! task's panic, also while the thread is unwinding from an unrelated panic,
//! and a panic of the runtime itself in the task, followed by the factory's
//! drop panic, does not abort either. Each scenario runs in a child process
//! of this test binary, so an abort fails one test instead of ending the
//! binary.

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(feature = "tracing")]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, StopReason, StreamEvent,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);

/// Set right before the actor stops itself: from then on the parked value
/// and its owner panic when dropped. Each scenario runs alone in its own
/// process, so one flag serves it.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The text of the parked value's drop panic.
const PARKED: &str = "the parked value panicked";

/// The text of the owner's drop panic: the recurring factory's capture or
/// the stream.
const OWNER: &str = "the owner panicked";

/// Panics with [`OWNER`] when dropped once armed.
struct Owner;

impl Drop for Owner {
    fn drop(&mut self) {
        if ARMED.load(Ordering::SeqCst) {
            panic!("{OWNER}");
        }
    }
}

/// The `n`th message a forwarder built; from the third on it panics with
/// [`PARKED`] when dropped once armed. The first is handled, the second
/// fills the one slot of the mailbox, the third is parked in the send.
struct Item(usize);

impl Drop for Item {
    fn drop(&mut self) {
        if self.0 >= 3 && ARMED.load(Ordering::SeqCst) {
            panic!("{PARKED}");
        }
    }
}

/// How many items the forwarder built.
static BUILT: AtomicUsize = AtomicUsize::new(0);

fn next_item() -> Item {
    Item(BUILT.fetch_add(1, Ordering::SeqCst) + 1)
}

impl From<StreamEvent<()>> for Item {
    fn from(_: StreamEvent<()>) -> Self {
        next_item()
    }
}

/// A stream that always has an item and owns an [`Owner`].
struct Flooding(#[allow(dead_code)] Owner);

impl Stream for Flooding {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        Poll::Ready(Some(()))
    }
}

/// Starts a recurring timer or a stream whose items fill its one-slot
/// mailbox. Its first turn waits until the third item is built, parked in
/// the forwarder's send, then arms the drops and stops the actor.
struct Parked {
    stream: bool,
}

impl Actor for Parked {
    type Message = Item;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.stream {
            ctx.add_stream(Flooding(Owner));
        } else {
            let owner = Owner;
            ctx.every_with(Duration::from_millis(5), move || {
                let _ = &owner;
                next_item()
            })
            .await
            .expect("the timer starts");
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: Item, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        // The third item is built right before its send, with no await
        // between, and the second fills the mailbox while this turn runs.
        while BUILT.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        ARMED.store(true, Ordering::SeqCst);
        ctx.self_handle()
            .stop(StopReason::Graceful)
            .await
            .expect("accepted");
        Ok(())
    }
}

fn on(flavor: &str, body: impl Future<Output = ()>) {
    let runtime = match flavor {
        "current_thread" => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
        _ => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build(),
    }
    .expect("a runtime");
    runtime.block_on(body);
}

fn a_parked_forwarder_with_two_panicking_drops_ends_in_one_reason(stream: bool, flavor: &str) {
    #[cfg(feature = "tracing")]
    let logged = warnings::install();
    on(flavor, async {
        let actor = Parked { stream }
            .spawn()
            .with_config(ActorConfig::default().with_mailbox_capacity(1))
            .await
            .expect("the actor starts");
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends");
        assert_eq!(
            reason,
            StopReason::Failure(ActorError::Panic(PARKED.to_string())),
            "the parked value's panic is the end's"
        );
        #[cfg(feature = "tracing")]
        {
            let token = format!("actor_id={} ", actor.id());
            let lines: Vec<String> = logged
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains(&token) && line.contains(OWNER))
                .cloned()
                .collect();
            assert_eq!(
                lines.len(),
                1,
                "the owner's panic is logged once: {lines:?}"
            );
            assert!(lines[0].contains("discarded"), "{lines:?}");
        }
    });
}

#[test]
fn a_parked_recurring_tick_and_its_factory_both_panicking_end_in_one_reason_current_thread() {
    if common::in_a_child_process(
        "a_parked_recurring_tick_and_its_factory_both_panicking_end_in_one_reason_current_thread",
    ) {
        a_parked_forwarder_with_two_panicking_drops_ends_in_one_reason(false, "current_thread");
    }
}

#[test]
fn a_parked_recurring_tick_and_its_factory_both_panicking_end_in_one_reason_multi_thread() {
    if common::in_a_child_process(
        "a_parked_recurring_tick_and_its_factory_both_panicking_end_in_one_reason_multi_thread",
    ) {
        a_parked_forwarder_with_two_panicking_drops_ends_in_one_reason(false, "multi_thread");
    }
}

#[test]
fn a_parked_stream_item_and_its_stream_both_panicking_end_in_one_reason_current_thread() {
    if common::in_a_child_process(
        "a_parked_stream_item_and_its_stream_both_panicking_end_in_one_reason_current_thread",
    ) {
        a_parked_forwarder_with_two_panicking_drops_ends_in_one_reason(true, "current_thread");
    }
}

#[test]
fn a_parked_stream_item_and_its_stream_both_panicking_end_in_one_reason_multi_thread() {
    if common::in_a_child_process(
        "a_parked_stream_item_and_its_stream_both_panicking_end_in_one_reason_multi_thread",
    ) {
        a_parked_forwarder_with_two_panicking_drops_ends_in_one_reason(true, "multi_thread");
    }
}

/// A stream that never yields and owns an [`Owner`].
struct Silent(#[allow(dead_code)] Owner);

impl Stream for Silent {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        Poll::Pending
    }
}

/// Holds a recurring timer that never fires or a stream that never yields,
/// either owning an [`Owner`]. Answers each message with the last error its
/// timers and streams left.
struct Holder {
    stream: bool,
}

impl Actor for Holder {
    type Message = Item;
    type Response = Option<String>;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.stream {
            ctx.add_stream(Silent(Owner));
        } else {
            let owner = Owner;
            ctx.every_with(Duration::from_secs(3600), move || {
                let _ = &owner;
                next_item()
            })
            .await
            .expect("the timer starts");
        }
        Ok(())
    }

    async fn handle(
        &mut self,
        _msg: Item,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Option<String>> {
        Ok(ctx.last_forwarder_error().map(ToString::to_string))
    }
}

/// Stops the actor on `runtime`, its owner's drop armed, and returns its
/// reason.
fn stop_and_read(runtime: &tokio::runtime::Runtime, actor: ActorHandle<Holder>) -> StopReason {
    runtime.block_on(async move {
        ARMED.store(true, Ordering::SeqCst);
        actor.stop(StopReason::Graceful).await.expect("accepted");
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends");
        ARMED.store(false, Ordering::SeqCst);
        reason
    })
}

/// Stops its actor when dropped, recording whether the thread was unwinding
/// then, and the actor's reason.
struct Fixture {
    runtime: Option<tokio::runtime::Runtime>,
    actor: Option<ActorHandle<Holder>>,
    out: Arc<Mutex<Option<(bool, StopReason)>>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let unwinding = std::thread::panicking();
        let runtime = self.runtime.take().unwrap();
        let reason = stop_and_read(&runtime, self.actor.take().unwrap());
        *self.out.lock().unwrap() = Some((unwinding, reason));
    }
}

/// A recurring timer's factory or a stream whose drop panics alone, dropped
/// by the end's abort, is the task's panic: the actor's reason is that
/// panic, whether or not the thread is unwinding from an unrelated panic at
/// that moment (a runtime driven from a `Drop` during that unwind).
fn a_lone_owner_drop_panic_is_the_task_s(stream: bool, ambient: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let actor =
        runtime.block_on(async { Holder { stream }.spawn().await.expect("the actor starts") });
    let out = Arc::new(Mutex::new(None));
    let fixture = Fixture {
        runtime: Some(runtime),
        actor: Some(actor),
        out: out.clone(),
    };
    if ambient {
        let unwound = catch_unwind(AssertUnwindSafe(move || {
            let _fixture = fixture;
            panic!("an unrelated panic");
        }));
        assert!(unwound.is_err());
    } else {
        drop(fixture);
    }
    let (unwinding, reason) = out.lock().unwrap().take().expect("the fixture ran");
    assert_eq!(unwinding, ambient);
    assert_eq!(
        reason,
        StopReason::Failure(ActorError::Panic(OWNER.to_string())),
        "the owner's drop panic, alone, is the task's panic"
    );
}

#[test]
fn a_lone_factory_drop_panic_is_the_task_s() {
    if common::in_a_child_process("a_lone_factory_drop_panic_is_the_task_s") {
        a_lone_owner_drop_panic_is_the_task_s(false, false);
    }
}

#[test]
fn a_lone_factory_drop_panic_during_an_unrelated_unwind_is_the_task_s() {
    if common::in_a_child_process(
        "a_lone_factory_drop_panic_during_an_unrelated_unwind_is_the_task_s",
    ) {
        a_lone_owner_drop_panic_is_the_task_s(false, true);
    }
}

#[test]
fn a_lone_stream_drop_panic_is_the_task_s() {
    if common::in_a_child_process("a_lone_stream_drop_panic_is_the_task_s") {
        a_lone_owner_drop_panic_is_the_task_s(true, false);
    }
}

#[test]
fn a_lone_stream_drop_panic_during_an_unrelated_unwind_is_the_task_s() {
    if common::in_a_child_process(
        "a_lone_stream_drop_panic_during_an_unrelated_unwind_is_the_task_s",
    ) {
        a_lone_owner_drop_panic_is_the_task_s(true, true);
    }
}

/// Set by a test whose log subscriber panics on every line that records an
/// ended stream's panic its forwarder did not report.
#[cfg(feature = "tracing")]
static PANIC_ON_ENDED: AtomicBool = AtomicBool::new(false);

/// Set by a test whose log subscriber panics once, on the first line that
/// says a panic was discarded.
#[cfg(feature = "tracing")]
static PANIC_ON_DISCARDED: AtomicBool = AtomicBool::new(false);

/// The text of that subscriber's panic.
#[cfg(feature = "tracing")]
const SUBSCRIBER: &str = "the log subscriber panicked";

/// Runs a recurring timer on a runtime without the time driver: its first
/// poll panics inside the runtime's sleep while the factory is held, and
/// the factory's drop panics during that unwind. Returns the actor's id and
/// the last error its timers left.
fn a_timer_without_a_time_driver() -> (String, String) {
    ARMED.store(true, Ordering::SeqCst);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime without the time driver");
    let (name, error) = runtime.block_on(async {
        let actor = Holder { stream: false }
            .spawn()
            .await
            .expect("the actor starts");
        // The timer's task panics at its first poll; the actor's loop reaps
        // it between two requests, so a later request reads its error.
        let mut error = None;
        for _ in 0..1000 {
            tokio::task::yield_now().await;
            error = actor.send(Item(0)).await.expect("the actor answers");
            if error.is_some() {
                break;
            }
        }
        (actor.id().to_string(), error)
    });
    ARMED.store(false, Ordering::SeqCst);
    std::mem::forget(runtime);
    (name, error.expect("the timer's task panicked"))
}

/// The runtime's panic and the factory's drop panic in one timer's task do
/// not abort: the task's panic is the runtime's, and with the `tracing`
/// feature the factory's is logged once, naming the actor.
#[test]
fn a_runtime_panic_and_a_factory_drop_panic_in_one_timer_do_not_abort() {
    if !common::in_a_child_process(
        "a_runtime_panic_and_a_factory_drop_panic_in_one_timer_do_not_abort",
    ) {
        return;
    }
    #[cfg(feature = "tracing")]
    let logged = warnings::install();
    let (name, error) = a_timer_without_a_time_driver();
    assert!(error.contains("timer"), "the runtime's panic: {error}");
    #[cfg(feature = "tracing")]
    {
        let token = format!("actor_id={name} ");
        let lines: Vec<String> = logged
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(&token) && line.contains(OWNER))
            .cloned()
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "the factory's panic is logged once: {lines:?}"
        );
    }
    #[cfg(not(feature = "tracing"))]
    let _ = name;
}

/// The same timer, with a log subscriber that panics on the line that
/// records the factory's discarded panic: the line is written, the
/// subscriber's panic is contained there, the process does not abort, and
/// the task's panic stays the runtime's.
#[cfg(feature = "tracing")]
#[test]
fn a_panicking_log_of_a_discarded_panic_does_not_abort() {
    if !common::in_a_child_process("a_panicking_log_of_a_discarded_panic_does_not_abort") {
        return;
    }
    let logged = warnings::install();
    PANIC_ON_DISCARDED.store(true, Ordering::SeqCst);
    let (name, error) = a_timer_without_a_time_driver();
    assert!(error.contains("timer"), "the runtime's panic: {error}");
    let token = format!("actor_id={name} ");
    let lines: Vec<String> = logged
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(&token) && line.contains(OWNER))
        .cloned()
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "the factory's panic is logged once: {lines:?}"
    );
}

/// Set by the stream that ends at once, when its drop ran.
static ENDED_STREAM_DROPPED: AtomicBool = AtomicBool::new(false);

/// Set to make the conversion into the `Finished` message panic.
static FINISHED_FROM_PANICS: AtomicBool = AtomicBool::new(false);

/// The text of the ended stream's drop panic.
const ENDED: &str = "the ended stream panicked";

/// The text of the panicking conversion into `Finished`.
const FINISHED_FROM: &str = "the conversion into Finished panicked";

/// The text of the `Finished` message's drop panic.
const FINISHED_DROP: &str = "the Finished message panicked";

/// Panics with [`ENDED`] when dropped once armed, recording its drop.
struct EndedOwner;

impl Drop for EndedOwner {
    fn drop(&mut self) {
        if ARMED.load(Ordering::SeqCst) {
            ENDED_STREAM_DROPPED.store(true, Ordering::SeqCst);
            panic!("{ENDED}");
        }
    }
}

/// A stream that ends at once and owns an [`EndedOwner`].
struct Ending(#[allow(dead_code)] EndedOwner);

impl Stream for Ending {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<()>> {
        Poll::Ready(None)
    }
}

/// Panics with [`FINISHED_DROP`] when dropped once armed.
struct FinishedBomb;

impl Drop for FinishedBomb {
    fn drop(&mut self) {
        if ARMED.load(Ordering::SeqCst) {
            panic!("{FINISHED_DROP}");
        }
    }
}

enum EndMsg {
    /// Starts the ending stream, waits until its drop ran, and, with
    /// `stop`, stops the actor.
    Start,
    Filler,
    Read,
    Data,
    Finished(#[allow(dead_code)] FinishedBomb),
}

impl From<StreamEvent<()>> for EndMsg {
    fn from(event: StreamEvent<()>) -> Self {
        match event {
            StreamEvent::Data(()) => EndMsg::Data,
            StreamEvent::Finished => {
                if FINISHED_FROM_PANICS.load(Ordering::SeqCst) {
                    panic!("{FINISHED_FROM}");
                }
                EndMsg::Finished(FinishedBomb)
            }
        }
    }
}

/// Starts a stream that ends during the run with a panicking drop. With
/// `stop`, its one-slot mailbox is full first, so the stream's `Finished`
/// waits for room, and the actor then stops itself.
struct Ender {
    stop: bool,
}

impl Actor for Ender {
    type Message = EndMsg;
    type Response = Option<String>;

    async fn handle(
        &mut self,
        msg: EndMsg,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Option<String>> {
        match msg {
            EndMsg::Start => {
                if self.stop {
                    assert!(ctx.self_handle().try_notify(EndMsg::Filler).is_ok());
                }
                ARMED.store(true, Ordering::SeqCst);
                ctx.add_stream(Ending(EndedOwner));
                // The stream's drop and what follows it (the conversion,
                // the wait for room) run in one poll of its task.
                while !ENDED_STREAM_DROPPED.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                if !self.stop {
                    // The `Finished`, if it was sent, drops quietly when
                    // handled.
                    ARMED.store(false, Ordering::SeqCst);
                }
                if self.stop {
                    ctx.self_handle()
                        .stop(StopReason::Graceful)
                        .await
                        .expect("accepted");
                }
                Ok(None)
            }
            EndMsg::Read => Ok(ctx.last_forwarder_error().map(ToString::to_string)),
            _ => Ok(None),
        }
    }
}

/// A stream ends during the run with a panicking drop, then the conversion
/// into its `Finished` panics. Returns the actor's id and the last error its
/// streams left.
fn an_ended_stream_whose_finished_conversion_panics() -> (String, String) {
    FINISHED_FROM_PANICS.store(true, Ordering::SeqCst);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let (name, error) = runtime.block_on(async {
        let actor = Ender { stop: false }
            .spawn()
            .await
            .expect("the actor starts");
        actor.send(EndMsg::Start).await.expect("the stream ends");
        // The stream's task is reaped between two requests.
        let mut error = None;
        for _ in 0..1000 {
            error = actor.send(EndMsg::Read).await.expect("the actor answers");
            if error.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        (actor.id().to_string(), error)
    });
    ARMED.store(false, Ordering::SeqCst);
    (name, error.expect("the stream's task panicked"))
}

/// A stream ends during the run with a panicking drop, and its `Finished`
/// is sent: the drop's panic is reported with it, as the last forwarder
/// error, and with the `tracing` feature it is not logged as an unreported
/// panic.
#[test]
fn an_ended_stream_s_reported_drop_panic_is_not_logged_as_unreported() {
    if !common::in_a_child_process(
        "an_ended_stream_s_reported_drop_panic_is_not_logged_as_unreported",
    ) {
        return;
    }
    #[cfg(feature = "tracing")]
    let logged = warnings::install();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let (name, error) = runtime.block_on(async {
        let actor = Ender { stop: false }
            .spawn()
            .await
            .expect("the actor starts");
        actor.send(EndMsg::Start).await.expect("the stream ends");
        let mut error = None;
        for _ in 0..1000 {
            error = actor.send(EndMsg::Read).await.expect("the actor answers");
            if error.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        (actor.id().to_string(), error)
    });
    let error = error.expect("the stream's drop panic is reported");
    assert!(error.contains(ENDED), "the ended stream's panic: {error}");
    #[cfg(feature = "tracing")]
    {
        let token = format!("actor_id={name} ");
        let lines: Vec<String> = logged
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(&token) && line.contains("ended before reporting"))
            .cloned()
            .collect();
        assert!(lines.is_empty(), "{lines:?}");
    }
    #[cfg(not(feature = "tracing"))]
    let _ = name;
}

/// The conversion's panic is the stream's task panic, the last forwarder
/// error, and with the `tracing` feature the ended stream's drop panic is
/// logged once, naming the actor.
#[test]
fn an_ended_stream_s_drop_panic_is_logged_once_when_its_finished_conversion_panics() {
    if !common::in_a_child_process(
        "an_ended_stream_s_drop_panic_is_logged_once_when_its_finished_conversion_panics",
    ) {
        return;
    }
    #[cfg(feature = "tracing")]
    let logged = warnings::install();
    let (name, error) = an_ended_stream_whose_finished_conversion_panics();
    assert!(
        error.contains(FINISHED_FROM),
        "the conversion's panic: {error}"
    );
    #[cfg(feature = "tracing")]
    {
        let token = format!("actor_id={name} ");
        let lines: Vec<String> = logged
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(&token) && line.contains(ENDED))
            .cloned()
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "the ended stream's panic is logged once: {lines:?}"
        );
        assert!(lines[0].contains("ended before reporting"), "{lines:?}");
    }
    #[cfg(not(feature = "tracing"))]
    let _ = name;
}

/// The same, with a log subscriber that panics on the ended stream's line,
/// which is written while the conversion's panic unwinds: the log's panic is
/// caught and discarded, and the process does not abort.
#[cfg(feature = "tracing")]
#[test]
fn a_panicking_log_of_an_ended_stream_s_panic_during_a_conversion_panic_does_not_abort() {
    if !common::in_a_child_process(
        "a_panicking_log_of_an_ended_stream_s_panic_during_a_conversion_panic_does_not_abort",
    ) {
        return;
    }
    let _ = warnings::install();
    PANIC_ON_ENDED.store(true, Ordering::SeqCst);
    let (_, error) = an_ended_stream_whose_finished_conversion_panics();
    assert!(
        error.contains(FINISHED_FROM),
        "the conversion's panic: {error}"
    );
}

/// A stream ends during the run with a panicking drop while the mailbox is
/// full, its `Finished` waits for room, the actor stops, and the end's
/// abort drops the `Finished`, whose drop panics; a log subscriber panics on
/// the ended stream's line, written during that unwind. The log's panic is
/// caught and discarded, the process does not abort, and the actor's reason
/// is the `Finished` message's drop panic.
#[cfg(feature = "tracing")]
#[test]
fn a_panicking_log_of_an_ended_stream_s_panic_at_the_end_s_abort_does_not_abort() {
    if !common::in_a_child_process(
        "a_panicking_log_of_an_ended_stream_s_panic_at_the_end_s_abort_does_not_abort",
    ) {
        return;
    }
    let logged = warnings::install();
    PANIC_ON_ENDED.store(true, Ordering::SeqCst);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let (name, reason) = runtime.block_on(async {
        let actor = Ender { stop: true }
            .spawn()
            .with_config(ActorConfig::default().with_mailbox_capacity(1))
            .await
            .expect("the actor starts");
        actor.notify(EndMsg::Start).await.expect("queued");
        let reason = timeout(BOUND, actor.wait_stopped())
            .await
            .expect("the actor ends");
        (actor.id().to_string(), reason)
    });
    ARMED.store(false, Ordering::SeqCst);
    assert_eq!(
        reason,
        StopReason::Failure(ActorError::Panic(FINISHED_DROP.to_string()))
    );
    // The holder's line was written, once, before the subscriber panicked.
    let token = format!("actor_id={name} ");
    let lines: Vec<String> = logged
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(&token) && line.contains("ended before reporting"))
        .cloned()
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
}

#[cfg(feature = "tracing")]
mod warnings {
    use std::fmt::Write as _;

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    use super::{Arc, Mutex, OnceLock};

    /// Collects every WARN event as one line of `name=value` pairs.
    #[derive(Clone, Default)]
    struct Warnings {
        lines: Arc<Mutex<Vec<String>>>,
    }

    /// Installs the one global subscriber of this process on first use and
    /// returns its lines.
    pub fn install() -> Arc<Mutex<Vec<String>>> {
        static GLOBAL: OnceLock<Warnings> = OnceLock::new();
        GLOBAL
            .get_or_init(|| {
                let warnings = Warnings::default();
                tracing::subscriber::set_global_default(warnings.clone())
                    .expect("no other global subscriber is installed in this process");
                warnings
            })
            .lines
            .clone()
    }

    struct Text(String);

    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    impl Subscriber for Warnings {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == Level::WARN
        }

        fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut text = Text(String::new());
            event.record(&mut text);
            let discarded = text.0.contains("discarded");
            let ended = text.0.contains("ended before reporting");
            self.lines.lock().unwrap().push(text.0);
            if discarded && super::PANIC_ON_DISCARDED.swap(false, super::Ordering::SeqCst) {
                panic!("{}", super::SUBSCRIBER);
            }
            if ended && super::PANIC_ON_ENDED.load(super::Ordering::SeqCst) {
                panic!("{}", super::SUBSCRIBER);
            }
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }
}
