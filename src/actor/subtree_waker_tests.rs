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

/// A wait on `released()` parked with a waker that panics when woken: the
/// release of the hold wakes it from the hold's drop, and the panic stays out
/// of that drop, whichever list the wait is on.
#[test]
fn the_release_of_a_hold_wakes_a_waker_that_panics_without_unwinding_the_drop() {
    for observer in [false, true] {
        let root = SubtreeHold::new(None);
        let mut watch = root.watch();
        watch.observer = observer;
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut cx = Context::from_waker(&waker);
        let mut released = Box::pin(watch.released());
        assert!(released.as_mut().poll(&mut cx).is_pending());
        loud.arm(true);
        let dropped = catch_unwind(AssertUnwindSafe(|| drop(root)));
        loud.arm(false);
        assert_eq!(loud.woken(), 1, "the wait was woken once");
        assert!(
            dropped.is_ok(),
            "a waker's panic unwound out of the hold's drop"
        );
        assert!(released.as_mut().poll(&mut cx).is_ready());
    }
}

/// A wait on the end of the subtree parked with a waker that panics when
/// woken: the end of the last actor below wakes it from that actor's drop, and
/// the panic stays out of that drop, whichever list the wait is on.
#[tokio::test(start_paused = true)]
async fn the_end_of_the_last_actor_below_wakes_a_waker_that_panics_without_unwinding_the_drop() {
    for observer in [false, true] {
        let root = SubtreeHold::new(None);
        let kid = SubtreeHold::new(Some(SubtreeLink {
            parent: root.token(),
            own: ms(10),
            infinity: false,
        }));
        let mut watch = root.watch();
        watch.observer = observer;
        drop(root);
        assert!(watch.is_released());
        assert!(!watch.is_gone());
        let mut wait = SubtreeWait::new(&watch);
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut cx = Context::from_waker(&waker);
        let mut changed = Box::pin(wait.changed());
        assert!(changed.as_mut().poll(&mut cx).is_pending());
        loud.arm(true);
        let dropped = catch_unwind(AssertUnwindSafe(|| drop(kid)));
        loud.arm(false);
        assert_eq!(loud.woken(), 1, "the wait was woken once");
        assert!(
            dropped.is_ok(),
            "a waker's panic unwound out of the last actor's drop"
        );
        assert!(changed.as_mut().poll(&mut cx).is_ready());
    }
}

/// The release of a hold on a thread that is already unwinding wakes a waker
/// that panics. A panic that escaped the drop would be a second panic during
/// the unwind and end the process, so the scenario runs in a child process.
#[test]
fn the_release_of_a_hold_during_an_unwind_wakes_a_waker_that_panics_without_ending_the_process() {
    if !crate::actor::panic::in_a_child_process(
        module_path!(),
        "the_release_of_a_hold_during_an_unwind_wakes_a_waker_that_panics_without_ending_the_process",
    ) {
        return;
    }
    for observer in [false, true] {
        let root = SubtreeHold::new(None);
        let mut watch = root.watch();
        watch.observer = observer;
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut cx = Context::from_waker(&waker);
        let mut released = Box::pin(watch.released());
        assert!(released.as_mut().poll(&mut cx).is_pending());
        loud.arm(true);
        let unwound = std::thread::spawn(move || {
            let _owned = root;
            panic!("the thread unwinds");
        })
        .join();
        loud.arm(false);
        assert!(unwound.is_err(), "the thread ended by its own panic");
        assert_eq!(loud.woken(), 1, "the wait was woken once");
    }
}

/// The end of the last actor below, on a thread that is already unwinding,
/// wakes a waker that panics: the child process shows no second panic.
#[test]
fn the_end_of_the_last_actor_below_during_an_unwind_wakes_a_waker_that_panics_without_ending_the_process(
) {
    if !crate::actor::panic::in_a_child_process(
        module_path!(),
        "the_end_of_the_last_actor_below_during_an_unwind_wakes_a_waker_that_panics_without_ending_the_process",
    ) {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a paused current-thread runtime");
    let _entered = runtime.enter();
    for observer in [false, true] {
        let root = SubtreeHold::new(None);
        let kid = SubtreeHold::new(Some(SubtreeLink {
            parent: root.token(),
            own: ms(10),
            infinity: true,
        }));
        let mut watch = root.watch();
        watch.observer = observer;
        drop(root);
        let mut wait = SubtreeWait::new(&watch);
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut cx = Context::from_waker(&waker);
        let mut changed = Box::pin(wait.changed());
        assert!(changed.as_mut().poll(&mut cx).is_pending());
        loud.arm(true);
        let unwound = std::thread::spawn(move || {
            let _owned = kid;
            panic!("the thread unwinds");
        })
        .join();
        loud.arm(false);
        assert!(unwound.is_err(), "the thread ended by its own panic");
        assert_eq!(loud.woken(), 1, "the wait was woken once");
    }
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

/// An observer's wait whose waker panics, and the library's own wait on the
/// same subtree, parked in either order. Returns how many times each was woken.
fn library_wait_beside_a_panicking_observer(observer_parks_first: bool) -> (usize, usize) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a paused current-thread runtime");
    let _entered = runtime.enter();
    let (watch, kid) = unbounded_subtree();
    let observer = Arc::new(LoudWake::default());
    let library = Arc::new(LoudWake::default());
    let (observer_waker, library_waker) =
        (Waker::from(observer.clone()), Waker::from(library.clone()));
    let mut observer_wait = SubtreeWait::new(&watch.clone().for_observer());
    let mut library_wait = SubtreeWait::new(&watch);
    let mut observer_cx = Context::from_waker(&observer_waker);
    let mut library_cx = Context::from_waker(&library_waker);
    let mut observer_changed = Box::pin(observer_wait.changed());
    let mut library_changed = Box::pin(library_wait.changed());
    if observer_parks_first {
        assert!(observer_changed
            .as_mut()
            .poll(&mut observer_cx)
            .is_pending());
        assert!(library_changed.as_mut().poll(&mut library_cx).is_pending());
    } else {
        assert!(library_changed.as_mut().poll(&mut library_cx).is_pending());
        assert!(observer_changed
            .as_mut()
            .poll(&mut observer_cx)
            .is_pending());
    }
    observer.arm(true);
    let dropped = catch_unwind(AssertUnwindSafe(|| drop(kid)));
    observer.arm(false);
    assert!(dropped.is_ok(), "a waker's panic unwound out of the drop");
    (observer.woken(), library.woken())
}

/// A subtree observer's waker that panics when woken does not strand a wait of
/// the library on the same subtree, whichever of the two parked first.
#[test]
fn a_panicking_observer_waker_does_not_strand_a_library_wait_that_parked_after_it() {
    let (observer, library) = library_wait_beside_a_panicking_observer(true);
    assert_eq!(observer, 1, "the observer's waker was woken once");
    assert_eq!(library, 1, "the library's wait was left unwoken");
}

/// The same, with the library's wait parked before the observer's.
#[test]
fn a_panicking_observer_waker_does_not_strand_a_library_wait_that_parked_before_it() {
    let (observer, library) = library_wait_beside_a_panicking_observer(false);
    assert_eq!(observer, 1, "the observer's waker was woken once");
    assert_eq!(library, 1, "the library's wait was left unwoken");
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
