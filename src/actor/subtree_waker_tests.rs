//! The subtree's signals wake the tasks of subtree observers, whose wakers
//! belong to those tasks. A panic of one stays out of the drop that changed
//! the signal, also when that drop runs during an unwind, and out of the
//! library's own waits on the same subtree, whichever of the two parked first.
//! The observers behind a panicking waker finish at their next poll.

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Wake, Waker};

use super::*;

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: AtomicUsize,
}

impl LoudWake {
    fn arm(&self, armed: bool) {
        self.armed.store(armed, Ordering::SeqCst);
    }

    fn woken(&self) -> usize {
        self.woken.load(Ordering::SeqCst)
    }
}

impl Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a waker's wake panics");
        }
    }
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// One subtree with a supervisor child whose orderly stop has no bound, so a
/// wait on it has no deadline and parks on the signal alone.
fn unbounded_subtree() -> (SubtreeWatch, Arc<SubtreeHold>) {
    let root = SubtreeHold::new(None);
    let kid = SubtreeHold::new(Some(SubtreeLink {
        parent: root.token(),
        own: ms(10),
        infinity: true,
    }));
    let watch = root.watch();
    drop(root);
    (watch, kid)
}

/// Observers share one list. When the waker of the first panics, the wake of
/// the observers behind it is cut short, and they finish at their next poll.
#[test]
fn an_observer_left_unwoken_behind_a_panicking_waker_finishes_at_its_next_poll() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a paused current-thread runtime");
    let _entered = runtime.enter();
    let (watch, kid) = unbounded_subtree();
    let watch = watch.for_observer();
    let loud = Arc::new(LoudWake::default());
    let quiet = Arc::new(LoudWake::default());
    let (loud_waker, quiet_waker) = (Waker::from(loud.clone()), Waker::from(quiet.clone()));
    let mut loud_wait = SubtreeWait::new(&watch);
    let mut quiet_wait = SubtreeWait::new(&watch);
    let mut loud_cx = Context::from_waker(&loud_waker);
    let mut quiet_cx = Context::from_waker(&quiet_waker);
    let mut loud_changed = Box::pin(loud_wait.changed());
    let mut quiet_changed = Box::pin(quiet_wait.changed());
    assert!(loud_changed.as_mut().poll(&mut loud_cx).is_pending());
    assert!(quiet_changed.as_mut().poll(&mut quiet_cx).is_pending());

    loud.arm(true);
    let dropped = catch_unwind(AssertUnwindSafe(|| drop(kid)));
    loud.arm(false);
    assert!(dropped.is_ok(), "a waker's panic unwound out of the drop");
    assert_eq!(loud.woken(), 1, "the panicking waker was woken once");
    assert!(
        loud_changed.as_mut().poll(&mut loud_cx).is_ready(),
        "the observer whose waker panicked finishes at its next poll"
    );
    assert!(
        quiet_changed.as_mut().poll(&mut quiet_cx).is_ready(),
        "the observer behind it finishes at its next poll"
    );
}
