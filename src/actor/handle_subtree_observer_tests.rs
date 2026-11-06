//! The subtree observer on rigs built from the crate's own parts: a status
//! plane, a record and a hold made by hand, so a test decides when the actor
//! publishes `Stopped`, when its task lets go of the plane, and when its hold
//! is released, and a paused clock decides when the bound of the wait passes.

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::time::{timeout, Instant};

use super::SubtreeObserver;
use crate::actor::runtime::publish_stopped;
use crate::actor::subtree::{AbortRecord, SubtreeHold, SubtreeLink};
use crate::actor::supervision::{rules, ABORT_SETTLE};
use crate::types::{Shutdown, StatusRx, StatusTx, SubtreeEnd};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// One observed actor: its record, the sender of its plane, its hold, and an
/// observer built from the record's plane.
struct Rig {
    status: Option<StatusTx>,
    hold: Option<Arc<SubtreeHold>>,
    observer: SubtreeObserver,
}

impl Rig {
    /// An actor whose hold exists when the observer is built.
    fn new() -> Self {
        let record = AbortRecord::new();
        let (status, rx) = record.status_channel();
        let hold = SubtreeHold::with_record(None, record);
        Self {
            status: Some(status),
            hold: Some(hold),
            observer: SubtreeObserver::new(rx),
        }
    }

    /// A child of the actor with `own` stop budget; `infinity` marks a
    /// supervisor child whose orderly stop has no bound.
    fn child(&self, own: Duration, infinity: bool) -> Arc<SubtreeHold> {
        let parent = self.hold.as_ref().expect("the hold is held").token();
        SubtreeHold::new(Some(SubtreeLink {
            parent,
            own,
            infinity,
        }))
    }

    /// The actor publishes `Stopped`; its task still holds the plane and the
    /// hold.
    fn say_stopped(&self) {
        publish_stopped(self.status.as_ref().expect("the plane is open"));
    }

    /// The task lets go of the last sender: the plane closes.
    fn close_plane(&mut self) {
        self.status = None;
    }

    /// The task lets go of its hold.
    fn release_hold(&mut self) {
        self.hold = None;
    }

    /// The actor's whole end, in the order of its task: `Stopped`, the plane,
    /// the hold.
    fn end(&mut self) {
        self.say_stopped();
        self.close_plane();
        self.release_hold();
    }
}

/// The text of the panic a call ended with.
fn panic_text(joined: Result<impl Sized, tokio::task::JoinError>) -> String {
    match joined {
        Ok(_) => panic!("the call was expected to panic"),
        Err(err) => *err
            .into_panic()
            .downcast::<String>()
            .expect("a formatted panic message"),
    }
}

const NO_DRIVER: &str = "tokio-actors: SubtreeObserver::wait_ended needs the Tokio time driver on \
                         the calling runtime (Builder::enable_time or enable_all)";
const NO_RUNTIME: &str =
    "tokio-actors: SubtreeObserver::wait_ended needs a Tokio runtime with the \
                          time driver on the calling thread (Builder::enable_time or enable_all)";

/// Polls `call` once with no runtime current.
fn poll_once<F: Future>(call: &mut std::pin::Pin<&mut F>) -> Poll<F::Output> {
    let mut cx = Context::from_waker(Waker::noop());
    call.as_mut().poll(&mut cx)
}

/// The panic text of one poll made with no runtime current.
fn polled_panic<F: Future>(call: &mut std::pin::Pin<&mut F>) -> String {
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _ = poll_once(call);
    }))
    .expect_err("the poll panics");
    caught
        .downcast_ref::<String>()
        .expect("a formatted panic message")
        .clone()
}

#[tokio::test(start_paused = true)]
async fn an_observer_built_before_the_record_is_linked_waits_for_the_actor_then_the_subtree() {
    let record = AbortRecord::new();
    let (status, rx) = record.status_channel();
    // The registry shows an actor, and an observer can be built from it,
    // before the actor's hold links the record to its subtree.
    let observer = SubtreeObserver::new(rx);
    let hold = SubtreeHold::with_record(None, record.clone());
    let kid = SubtreeHold::new(Some(SubtreeLink {
        parent: hold.token(),
        own: ms(200),
        infinity: false,
    }));
    let mut wait = pin!(observer.wait_ended());
    assert!(
        timeout(Duration::from_secs(600), wait.as_mut())
            .await
            .is_err(),
        "the actor runs: the wait is pending however long it takes"
    );
    publish_stopped(&status);
    drop(status);
    drop(hold);
    assert!(
        timeout(ms(250), wait.as_mut()).await.is_err(),
        "the actor ended and the child below it lives, inside the bound"
    );
    drop(kid);
    assert_eq!(wait.await, SubtreeEnd::Ended);
}

#[tokio::test(start_paused = true)]
async fn the_wait_has_no_deadline_while_the_actor_runs() {
    let mut rig = Rig::new();
    let kid = rig.child(ms(200), false);
    let observer = rig.observer.clone();
    let mut wait = pin!(observer.wait_ended());
    assert!(
        timeout(Duration::from_secs(600), wait.as_mut())
            .await
            .is_err(),
        "ten minutes of a running actor are not a verdict"
    );
    rig.end();
    let ended = Instant::now();
    assert!(
        timeout(ms(299), wait.as_mut()).await.is_err(),
        "the bound counts from the actor's end, not from the call"
    );
    assert_eq!(wait.await, SubtreeEnd::Unresponsive);
    assert_eq!(ended.elapsed(), ms(200) + ABORT_SETTLE);
    drop(kid);
}

#[tokio::test(start_paused = true)]
async fn an_infinity_supervisor_below_holds_the_wait_without_a_bound() {
    let mut rig = Rig::new();
    let infinity = rig.child(ms(50), true);
    let leaf = SubtreeHold::new(Some(SubtreeLink {
        parent: infinity.token(),
        own: ms(200),
        infinity: false,
    }));
    rig.end();
    let mut wait = pin!(rig.observer.wait_ended());
    assert!(
        timeout(Duration::from_secs(3600), wait.as_mut())
            .await
            .is_err(),
        "no bound while a supervisor child under Infinity lives below"
    );
    // The Infinity child ends and its leaf lives: the bound arms from this
    // moment, as the longest chain below the actor plus the settle time.
    drop(infinity);
    let armed = Instant::now();
    assert!(timeout(ms(349), wait.as_mut()).await.is_err());
    assert_eq!(wait.await, SubtreeEnd::Unresponsive);
    assert_eq!(armed.elapsed(), ms(50 + 200) + ABORT_SETTLE);
    drop(leaf);
}

#[tokio::test(start_paused = true)]
async fn an_actor_below_that_never_ends_is_unresponsive_at_budget_plus_settle_and_a_later_call_answers_ended(
) {
    let mut rig = Rig::new();
    let kid = rig.child(ms(200), false);
    rig.end();
    let begun = Instant::now();
    let mut wait = pin!(rig.observer.wait_ended());
    assert!(
        timeout(ms(299), wait.as_mut()).await.is_err(),
        "no verdict before the budget and the settle time"
    );
    assert_eq!(wait.await, SubtreeEnd::Unresponsive);
    assert_eq!(begun.elapsed(), ms(200) + ABORT_SETTLE);

    // The wait stopped nothing: once the child ends, a later call answers at once.
    drop(kid);
    let later = Instant::now();
    assert_eq!(rig.observer.wait_ended().await, SubtreeEnd::Ended);
    assert_eq!(later.elapsed(), Duration::ZERO);
}

/// An abort issued against an actor below holds the verdict for the settle
/// time after it, past the bound.
#[tokio::test(start_paused = true)]
async fn an_abort_issued_below_holds_the_verdict_for_its_settle_time() {
    let mut rig = Rig::new();
    let record = AbortRecord::new();
    let kid = SubtreeHold::with_record(
        Some(SubtreeLink {
            parent: rig.hold.as_ref().expect("the hold is held").token(),
            own: ms(200),
            infinity: false,
        }),
        record.clone(),
    );
    let task = tokio::spawn(std::future::pending::<()>());
    record.owe();
    rig.end();
    let begun = Instant::now();
    let mut wait = pin!(rig.observer.wait_ended());
    assert!(
        timeout(ms(250), wait.as_mut()).await.is_err(),
        "inside the bound"
    );
    // The abort lands 250 ms in, 50 ms before the bound runs out: no verdict
    // until its settle time has passed.
    record.abort(&task.abort_handle());
    assert!(
        timeout(ms(99), wait.as_mut()).await.is_err(),
        "the bound passed at 300 ms but the abort has had 99 ms"
    );
    assert_eq!(wait.await, SubtreeEnd::Unresponsive);
    assert_eq!(begun.elapsed(), ms(250) + ABORT_SETTLE);
    drop(kid);
}

/// An abort that is owed and issued only after the bound has passed holds the
/// verdict until its own settle time is over: the wait wakes at the abort, and
/// does not answer at the bound it passed while the abort was still owed.
#[tokio::test(start_paused = true)]
async fn an_abort_issued_after_the_bound_holds_the_verdict_for_its_settle_time() {
    let mut rig = Rig::new();
    let record = AbortRecord::new();
    let kid = SubtreeHold::with_record(
        Some(SubtreeLink {
            parent: rig.hold.as_ref().expect("the hold is held").token(),
            own: ms(200),
            infinity: false,
        }),
        record.clone(),
    );
    let task = tokio::spawn(std::future::pending::<()>());
    record.owe();
    rig.end();
    let begun = Instant::now();
    let mut wait = pin!(rig.observer.wait_ended());
    assert!(
        timeout(ms(320), wait.as_mut()).await.is_err(),
        "the bound passed at 300 ms while the abort was still owed: no verdict"
    );
    record.abort(&task.abort_handle());
    assert!(
        timeout(ms(99), wait.as_mut()).await.is_err(),
        "the abort has had 99 ms of its settle time"
    );
    assert_eq!(wait.await, SubtreeEnd::Unresponsive);
    assert_eq!(begun.elapsed(), ms(320) + ABORT_SETTLE);
    drop(kid);
}

#[tokio::test(start_paused = true)]
async fn the_wait_does_not_count_the_actor_s_own_tail() {
    let mut rig = Rig::new();
    // `Stopped` is published, and the task has not let go of its plane or its
    // hold yet (the end log line of the tracing feature runs here): nothing
    // below the actor runs, and the actor has not ended.
    rig.say_stopped();
    let observer = rig.observer.clone();
    let mut wait = pin!(observer.wait_ended());
    assert!(
        timeout(Duration::from_secs(600), wait.as_mut())
            .await
            .is_err(),
        "the tail of the actor's own task is not a subtree that outlived its bound"
    );
    rig.close_plane();
    rig.release_hold();
    assert_eq!(wait.await, SubtreeEnd::Ended);
}

/// The moment between the plane's close and the hold's release is still the
/// actor's own end, which needs no timer: a call pends through it on any
/// runtime or none, the release wakes it, and it answers `Ended` at once.
#[test]
fn a_poll_between_the_close_of_the_plane_and_the_release_of_the_hold_needs_no_timer() {
    let mut rig = Rig::new();
    rig.say_stopped();
    rig.close_plane();
    let wakes = Arc::new(CountingWaker::default());
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);

    // No runtime is current.
    let observer = rig.observer.clone();
    let mut call = pin!(observer.wait_ended());
    assert!(call.as_mut().poll(&mut cx).is_pending());
    assert_eq!(wakes.count(), 0, "nothing woke the call yet");

    // A runtime without the time driver is current.
    let bare = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    let observer = rig.observer.clone();
    bare.block_on(async move {
        let mut call = pin!(observer.wait_ended());
        assert!(poll_once(&mut call).is_pending());
    });

    rig.release_hold();
    assert!(wakes.count() >= 1, "the release wakes the call");
    assert!(matches!(
        call.as_mut().poll(&mut cx),
        Poll::Ready(SubtreeEnd::Ended)
    ));
    let observer = rig.observer.clone();
    let mut call = pin!(observer.wait_ended());
    assert!(matches!(
        poll_once(&mut call),
        Poll::Ready(SubtreeEnd::Ended)
    ));
}

/// Once the hold is released, an actor still running below the observed one
/// is what the call needs a timer for: the same poll that ends the wait for
/// the hold reaches the check.
#[test]
fn a_call_that_finds_the_hold_released_and_an_actor_below_running_needs_a_timer() {
    let mut rig = Rig::new();
    let kid = rig.child(ms(200), false);
    rig.say_stopped();
    rig.close_plane();
    let wakes = Arc::new(CountingWaker::default());
    let waker = Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    let observer = rig.observer.clone();
    let mut call = pin!(observer.wait_ended());
    assert!(
        call.as_mut().poll(&mut cx).is_pending(),
        "the actor has not let go of its hold: no timer is needed yet"
    );
    rig.release_hold();
    assert!(wakes.count() >= 1, "the release wakes the call");
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _ = call.as_mut().poll(&mut cx);
    }))
    .expect_err("the child below lives and no timer exists");
    assert_eq!(
        caught
            .downcast_ref::<String>()
            .expect("a formatted panic message"),
        NO_RUNTIME
    );
    drop(kid);
}

/// Counts the wakes it is given.
#[derive(Default)]
struct CountingWaker(std::sync::atomic::AtomicUsize);

impl CountingWaker {
    fn count(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl std::task::Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn an_actor_that_never_got_a_hold_has_no_subtree_to_wait_for() {
    let record = AbortRecord::new();
    let (status, rx) = record.status_channel();
    let observer = SubtreeObserver::new(rx);
    let mut wait = pin!(observer.wait_ended());
    assert!(timeout(ms(10), wait.as_mut()).await.is_err());
    drop(status);
    assert_eq!(wait.await, SubtreeEnd::Ended);
}

#[tokio::test(start_paused = true)]
async fn a_budget_too_large_to_count_leaves_the_wait_without_a_bound() {
    let mut rig = Rig::new();
    // A kill grace of `Duration::MAX` reads as unbounded, and no supervisor
    // child under `Infinity` lives anywhere below.
    let own = rules::own_fin(false, Shutdown::Kill, Duration::MAX);
    assert_eq!(own, Duration::MAX);
    let kid = rig.child(own, false);
    rig.end();
    let mut wait = pin!(rig.observer.wait_ended());
    assert!(
        timeout(Duration::from_secs(365 * 86_400), wait.as_mut())
            .await
            .is_err(),
        "a year of waiting is not a verdict"
    );
    drop(kid);
    assert_eq!(wait.await, SubtreeEnd::Ended);
}

#[tokio::test(start_paused = true)]
async fn a_worker_s_shutdown_window_adds_nothing_to_the_bound_and_a_supervisor_child_s_does() {
    let window = Duration::from_secs(30);
    let grace = ms(120);
    for (is_supervisor, expected) in [
        (false, grace + ABORT_SETTLE + ABORT_SETTLE),
        (true, window + grace + ABORT_SETTLE + ABORT_SETTLE),
    ] {
        let mut rig = Rig::new();
        let own = rules::own_fin(is_supervisor, Shutdown::Timeout(window), grace);
        let kid = rig.child(own, false);
        rig.end();
        let begun = Instant::now();
        assert_eq!(rig.observer.wait_ended().await, SubtreeEnd::Unresponsive);
        assert_eq!(
            begun.elapsed(),
            expected,
            "supervisor child: {is_supervisor}"
        );
        drop(kid);
    }
}

#[test]
fn the_observer_keeps_no_token_alive() {
    let mut rig = Rig::new();
    let token = rig.hold.as_ref().expect("the hold is held").token();
    let weak = Arc::downgrade(&token);
    drop(token);
    rig.end();
    assert!(
        weak.upgrade().is_none(),
        "an observer of an ended subtree holds no token"
    );
    let observer = rig.observer.clone();
    let mut call = pin!(observer.wait_ended());
    assert!(matches!(
        poll_once(&mut call),
        Poll::Ready(SubtreeEnd::Ended)
    ));
}

/// The record still carries the signal of a subtree whose token is gone, so
/// "the subtree has ended" is read from the signal under its lock and not
/// inferred from a failed upgrade of the token.
#[test]
fn a_record_still_reads_the_signal_of_a_subtree_that_ended() {
    let mut rig = Rig::new();
    let kid = rig.child(ms(1), false);
    let rx = rig.observer.status_rx.clone();
    let watch = rx.subtree_watch().expect("the record is linked");
    rig.end();
    assert!(!watch.is_gone(), "the child below still lives");
    drop(kid);
    assert!(watch.is_gone());
    let later = rx.subtree_watch().expect("the record stays linked");
    assert!(later.is_gone(), "a watch taken after the end reads it too");
}

#[test]
fn an_observer_is_one_status_receiver() {
    assert_eq!(
        std::mem::size_of::<SubtreeObserver>(),
        std::mem::size_of::<StatusRx>(),
        "an observer holds a status receiver and nothing else"
    );
}

#[test]
fn an_observer_is_clone_send_sync_debug_and_static() {
    fn bounds<T: Clone + Send + Sync + std::fmt::Debug + 'static>() {}
    bounds::<SubtreeObserver>();
    let rig = Rig::new();
    assert_eq!(format!("{:?}", rig.observer), "SubtreeObserver { .. }");
}

#[test]
fn the_check_names_the_call_when_the_subtree_is_alive_and_no_timer_exists() {
    let mut rig = Rig::new();
    let kid = rig.child(ms(200), false);
    rig.end();

    // A calling runtime without the time driver.
    let bare = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    let observer = rig.observer.clone();
    let message = bare.block_on(async move {
        panic_text(tokio::spawn(async move { observer.wait_ended().await }).await)
    });
    assert_eq!(message, NO_DRIVER);

    // No runtime current.
    let observer = rig.observer.clone();
    let mut call = pin!(observer.wait_ended());
    assert_eq!(polled_panic(&mut call), NO_RUNTIME);

    // The subtree gone: no timer is needed, so nothing panics, on a runtime
    // without the driver or with none.
    drop(kid);
    let observer = rig.observer.clone();
    let answer = bare
        .block_on(async move { tokio::spawn(async move { observer.wait_ended().await }).await });
    assert_eq!(answer.expect("no panic"), SubtreeEnd::Ended);
    let observer = rig.observer.clone();
    let mut call = pin!(observer.wait_ended());
    assert!(matches!(
        poll_once(&mut call),
        Poll::Ready(SubtreeEnd::Ended)
    ));
}

#[test]
fn a_running_actor_needs_no_timer() {
    let rig = Rig::new();
    let _kid = rig.child(ms(200), false);
    let bare = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    let observer = rig.observer.clone();
    bare.block_on(async move {
        let mut call = pin!(observer.wait_ended());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(call.as_mut().poll(&mut cx).is_pending());
    });
    let observer = rig.observer.clone();
    let mut call = pin!(observer.wait_ended());
    assert!(poll_once(&mut call).is_pending());
}
