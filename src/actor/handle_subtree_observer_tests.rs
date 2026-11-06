//! The subtree observer on rigs built from the crate's own parts: a status
//! plane, a record and a hold made by hand, so a test decides when the actor
//! publishes `Stopped`, when its task lets go of the plane, and when its hold
//! is released, and a paused clock decides when the bound of the wait passes.

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Waker};
use std::time::Duration;

use super::SubtreeObserver;
use crate::actor::runtime::publish_stopped;
use crate::actor::subtree::{AbortRecord, SubtreeHold, SubtreeLink};
use crate::types::StatusTx;

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
}
const NO_RUNTIME: &str =
    "tokio-actors: SubtreeObserver::wait_ended needs a Tokio runtime with the \
                          time driver on the calling thread (Builder::enable_time or enable_all)";

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

#[test]
fn an_observer_is_clone_send_sync_debug_and_static() {
    fn bounds<T: Clone + Send + Sync + std::fmt::Debug + 'static>() {}
    bounds::<SubtreeObserver>();
    let rig = Rig::new();
    assert_eq!(format!("{:?}", rig.observer), "SubtreeObserver { .. }");
}
