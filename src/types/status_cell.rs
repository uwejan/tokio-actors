//! The status plane: one actor incarnation's lifecycle status, final reason
//! and terminal instant, kept in a cell inside the incarnation's shared
//! block ([`AbortRecord`]) together with a version, a count of the senders
//! that keep the plane open and a lock-free copy of the status byte.
//!
//! A [`StatusTx`] writes the cell, a [`StatusRx`] reads it and waits for it
//! to change. The cell keeps the latest value only: a reader that is not
//! waiting when two writes land sees the second one, and `Stopping` can be
//! missed.
//!
//! # Orderings every reader depends on
//!
//! | operation | order of its tests |
//! |---|---|
//! | [`StatusRx::has_changed`] | closed first: a closed plane answers `Err` even when a version is unseen |
//! | [`StatusRx::changed`] | version first: an unseen version answers `Ok` even when the plane is closed, then closed answers `Err` |
//! | [`StatusRx::wait_for`] | the predicate sees the current value first, always; after the plane closed it is evaluated once more only if a version was unseen |
//! | [`StatusRx::borrow`] | reads the value and never marks a version seen |
//! | every writer | changes the value, refreshes the status byte, then bumps the version, all in one critical section, then releases the lock, then wakes the waiters |
//!
//! The plane is closed exactly when the last [`StatusTx`] is dropped, however
//! many [`StatusRx`] and other holders of the block remain.
//!
//! # Locking
//!
//! The cell's lock is a leaf: nothing else is acquired while it is held and
//! no waker runs under it. [`StatusRef`] holds the lock for as long as it
//! lives, so it is `!Send`, `#[must_use]`, and never held across an `.await`
//! or while another cell call is made on the same block (the lock is not
//! reentrant). Under `cfg(test)` a second lock of one cell on one thread
//! panics instead of hanging, and so does a wake issued under the lock.
//! A lock poisoned by a panic elsewhere is ignored, as every other lock of
//! the block is.

use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};

use tokio::task::coop::cooperative;

use crate::actor::subtree::AbortRecord;
use crate::types::{ActorStatus, StatusValue};

/// Every sender of the plane is gone: the plane is closed and holds its
/// final value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecvError;

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("status plane closed")
    }
}

impl std::error::Error for RecvError {}

/// Bit 0 of the state word: the last sender is gone.
const CLOSED: u64 = 1;
/// One version in the state word (the version lives above the closed bit),
/// so one load gives a version and the closed bit that belong together.
const VERSION_STEP: u64 = 2;

fn encode(status: ActorStatus) -> u8 {
    match status {
        ActorStatus::Initializing => 0,
        ActorStatus::Running => 1,
        ActorStatus::Stopping => 2,
        ActorStatus::Stopped => 3,
    }
}

fn decode(byte: u8) -> ActorStatus {
    match byte {
        0 => ActorStatus::Initializing,
        1 => ActorStatus::Running,
        2 => ActorStatus::Stopping,
        _ => ActorStatus::Stopped,
    }
}

/// The plane's storage, one per incarnation.
#[derive(Debug)]
pub(crate) struct StatusCell {
    value: Mutex<StatusValue>,
    /// The version in bits 1 and up, the closed bit in bit 0. The version
    /// moves only inside the value's critical section; the closed bit is set
    /// by the last sender's drop.
    state: AtomicU64,
    /// Live [`StatusTx`] handles.
    senders: AtomicU32,
    /// The value's status, written in every critical section that writes the
    /// value, before that section moves the version, and read without the
    /// lock.
    mirror: AtomicU8,
}

impl StatusCell {
    pub(crate) fn new() -> Self {
        Self {
            value: Mutex::new(StatusValue::new(ActorStatus::Initializing)),
            state: AtomicU64::new(0),
            senders: AtomicU32::new(0),
            mirror: AtomicU8::new(encode(ActorStatus::Initializing)),
        }
    }

    fn lock(&self) -> CellGuard<'_> {
        #[cfg(test)]
        let key = self as *const Self as usize;
        #[cfg(test)]
        probe::enter(key);
        let guard = self.value.lock().unwrap_or_else(|p| p.into_inner());
        CellGuard {
            guard,
            #[cfg(test)]
            key,
        }
    }

    /// Whether the plane is closed and the version a reader has not seen:
    /// both from one load.
    fn load(&self) -> (u64, bool) {
        let state = self.state.load(Ordering::Acquire);
        (state / VERSION_STEP, state & CLOSED != 0)
    }
}

impl Default for StatusCell {
    fn default() -> Self {
        Self::new()
    }
}

/// The cell's lock held.
struct CellGuard<'a> {
    guard: MutexGuard<'a, StatusValue>,
    #[cfg(test)]
    key: usize,
}

impl Deref for CellGuard<'_> {
    type Target = StatusValue;

    fn deref(&self) -> &StatusValue {
        &self.guard
    }
}

impl DerefMut for CellGuard<'_> {
    fn deref_mut(&mut self) -> &mut StatusValue {
        &mut self.guard
    }
}

#[cfg(test)]
impl Drop for CellGuard<'_> {
    fn drop(&mut self) {
        probe::leave(self.key);
    }
}

/// A read of the plane's value. It holds the cell's lock: keep it short, do
/// not hold it across an `.await`, and make no other call on the same
/// plane while it lives.
#[must_use = "a StatusRef holds the plane's lock until it is dropped"]
pub(crate) struct StatusRef<'a>(CellGuard<'a>);

impl Deref for StatusRef<'_> {
    type Target = StatusValue;

    fn deref(&self) -> &StatusValue {
        &self.0
    }
}

impl AbortRecord {
    /// The incarnation's status plane: its first and only sender, and a
    /// receiver that has seen no version. Called once per record.
    pub(crate) fn status_channel(&self) -> (StatusTx, StatusRx) {
        let before = self.status_cell().senders.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(before, 0, "a record has one status plane");
        let tx = StatusTx {
            record: self.clone(),
        };
        let rx = tx.subscribe();
        (tx, rx)
    }

    /// Wakes every waiter on the block's signals. Never called with the
    /// status cell's lock held. A waiter's waker belongs to the waiter's task,
    /// so a panic of its wake is contained in the call (see
    /// [`AbortRecord::notify_signals`]): the write that woke it, the actor's
    /// own task and the drop of the last sender go on.
    fn wake_status_waiters(&self) {
        #[cfg(test)]
        probe::assert_free(self.status_cell() as *const StatusCell as usize);
        self.notify_signals();
    }
}

/// The writing half of the plane. Cloning adds a sender; the plane closes
/// when the last sender drops.
pub(crate) struct StatusTx {
    record: AbortRecord,
}

impl fmt::Debug for StatusTx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatusTx").finish_non_exhaustive()
    }
}

impl Clone for StatusTx {
    fn clone(&self) -> Self {
        // A live sender exists, so the count cannot be zero here.
        self.record
            .status_cell()
            .senders
            .fetch_add(1, Ordering::Relaxed);
        Self {
            record: self.record.clone(),
        }
    }
}

impl Drop for StatusTx {
    fn drop(&mut self) {
        let cell = self.record.status_cell();
        if cell.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The flag first, the wake after: a waiter that created its
            // notification before reading either sees the flag or is woken.
            cell.state.fetch_or(CLOSED, Ordering::Release);
            self.record.wake_status_waiters();
        }
    }
}

impl StatusTx {
    /// The plane's value. See [`StatusRef`] for what holding it means.
    pub(crate) fn borrow(&self) -> StatusRef<'_> {
        StatusRef(self.record.status_cell().lock())
    }

    /// The plane's current status, without the lock.
    pub(crate) fn status(&self) -> ActorStatus {
        decode(self.record.status_cell().mirror.load(Ordering::Acquire))
    }

    /// A receiver that has seen the current version.
    pub(crate) fn subscribe(&self) -> StatusRx {
        let (seen, _) = self.record.status_cell().load();
        StatusRx {
            record: self.record.clone(),
            seen,
        }
    }

    /// Runs `modify` on the value under the lock. When it returns `true` the
    /// version moves and the waiters wake. The status byte is refreshed
    /// either way, before the version moves. Succeeds with no receivers left.
    pub(crate) fn send_if_modified(&self, modify: impl FnOnce(&mut StatusValue) -> bool) -> bool {
        let cell = self.record.status_cell();
        let modified = {
            let mut guard = cell.lock();
            let modified = modify(&mut guard);
            // The byte first, the version after: a reader that consumed
            // the new version finds the byte of that value or a later one.
            cell.mirror.store(encode(guard.status), Ordering::Release);
            if modified {
                cell.state.fetch_add(VERSION_STEP, Ordering::Release);
            }
            modified
        };
        if modified {
            self.record.wake_status_waiters();
        }
        modified
    }

    /// Whether `other` reads this sender's plane.
    #[cfg(test)]
    pub(crate) fn same_channel(&self, other: &StatusRx) -> bool {
        self.record.same_block(&other.record)
    }

    /// Runs `modify` on the value, always moving the version.
    #[cfg(test)]
    pub(crate) fn send_modify(&self, modify: impl FnOnce(&mut StatusValue)) {
        self.send_if_modified(|value| {
            modify(value);
            true
        });
    }

    /// Replaces the whole value, always moving the version, and returns the
    /// old one.
    #[cfg(test)]
    pub(crate) fn send_replace(&self, new: StatusValue) -> StatusValue {
        let mut old = None;
        self.send_if_modified(|value| {
            old = Some(std::mem::replace(value, new));
            true
        });
        old.expect("the replacement ran")
    }

    /// A plane of its own, holding `init`.
    #[cfg(test)]
    pub(crate) fn channel(init: StatusValue) -> (StatusTx, StatusRx) {
        let record = AbortRecord::new();
        {
            let cell = record.status_cell();
            let mut guard = cell.lock();
            *guard = init;
            cell.mirror.store(encode(guard.status), Ordering::Release);
        }
        record.status_channel()
    }
}

/// The reading half of the plane. Cloning keeps what the clone has seen and
/// takes no lock; dropping it counts nothing.
pub(crate) struct StatusRx {
    record: AbortRecord,
    /// The last version this receiver saw.
    seen: u64,
}

impl fmt::Debug for StatusRx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatusRx")
            .field("seen", &self.seen)
            .finish_non_exhaustive()
    }
}

impl Clone for StatusRx {
    fn clone(&self) -> Self {
        Self {
            record: self.record.clone(),
            seen: self.seen,
        }
    }
}

impl StatusRx {
    /// The plane's value. It never marks a version seen. See [`StatusRef`]
    /// for what holding it means.
    pub(crate) fn borrow(&self) -> StatusRef<'_> {
        StatusRef(self.record.status_cell().lock())
    }

    /// The plane's current status, without the lock.
    pub(crate) fn status(&self) -> ActorStatus {
        decode(self.record.status_cell().mirror.load(Ordering::Acquire))
    }

    /// `Err` once the plane is closed, whether or not a version is unseen;
    /// otherwise whether a version is unseen. Takes no lock.
    pub(crate) fn has_changed(&self) -> Result<bool, RecvError> {
        let (version, closed) = self.record.status_cell().load();
        if closed {
            return Err(RecvError);
        }
        Ok(version != self.seen)
    }

    /// How many senders keep the plane open.
    #[cfg(test)]
    pub(crate) fn sender_count(&self) -> u32 {
        self.record.status_cell().senders.load(Ordering::Acquire)
    }

    /// Whether `other` reads the same plane: the same incarnation.
    pub(crate) fn same_channel(&self, other: &Self) -> bool {
        self.record.same_block(&other.record)
    }

    /// An observer of the subtree of the incarnation this receiver reads,
    /// or `None` while the incarnation's hold is not built yet. See
    /// [`AbortRecord::subtree_watch`].
    pub(crate) fn subtree_watch(&self) -> Option<crate::actor::subtree::SubtreeWatch> {
        self.record.subtree_watch()
    }

    /// Waits until a version this receiver has not seen exists and marks it
    /// seen; an unseen version answers `Ok` even on a closed plane. `Err`
    /// once the plane is closed and nothing is unseen.
    pub(crate) async fn changed(&mut self) -> Result<(), RecvError> {
        cooperative(changed_impl(&self.record, &mut self.seen)).await
    }

    /// Waits until `accept` returns `true` for the plane's value. The
    /// current value is tested first, whatever this receiver has seen. After
    /// the plane closed the last value is tested again only if it is a
    /// version this receiver had not seen, and then `Err` follows. Marks
    /// every version it tests as seen.
    pub(crate) async fn wait_for(
        &mut self,
        accept: impl FnMut(&StatusValue) -> bool,
    ) -> Result<(), RecvError> {
        cooperative(wait_for_impl(&self.record, &mut self.seen, accept)).await
    }
}

/// The version unseen by `seen` if there is one (marking it seen), else
/// `Err` if the plane is closed, else nothing yet.
fn maybe_changed(cell: &StatusCell, seen: &mut u64) -> Option<Result<(), RecvError>> {
    let (version, closed) = cell.load();
    if *seen != version {
        *seen = version;
        return Some(Ok(()));
    }
    if closed {
        return Some(Err(RecvError));
    }
    None
}

async fn changed_impl(record: &AbortRecord, seen: &mut u64) -> Result<(), RecvError> {
    loop {
        // The notification is created before the state is read, so a write
        // landing after the read wakes this wait. Another signal of the
        // block can wake it too; the loop reads again.
        let notified = record.signalled();
        if let Some(answer) = maybe_changed(record.status_cell(), seen) {
            return answer;
        }
        #[cfg(test)]
        probe::after_unchanged_read();
        notified.await;
    }
}

async fn wait_for_impl(
    record: &AbortRecord,
    seen: &mut u64,
    mut accept: impl FnMut(&StatusValue) -> bool,
) -> Result<(), RecvError> {
    let cell = record.status_cell();
    let mut closed = false;
    loop {
        let accepted = {
            let guard = cell.lock();
            let (version, _) = cell.load();
            let unseen = *seen != version;
            *seen = version;
            (!closed || unseen) && accept(&guard)
        };
        if accepted {
            return Ok(());
        }
        if closed {
            return Err(RecvError);
        }
        closed = changed_impl(record, seen).await.is_err();
    }
}

/// Probes of the cell's discipline, compiled into tests only.
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::RefCell;

    thread_local! {
        /// The cells this thread holds the lock of.
        static HELD: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
        /// Runs once, in the window between a wait's read of the state and
        /// its first poll of the notification.
        static AFTER_UNCHANGED_READ: RefCell<Option<Box<dyn FnOnce()>>> =
            const { RefCell::new(None) };
    }

    pub(super) fn enter(key: usize) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            assert!(
                !held.contains(&key),
                "status cell locked twice on one thread"
            );
            held.push(key);
        });
    }

    pub(super) fn leave(key: usize) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            if let Some(at) = held.iter().rposition(|k| *k == key) {
                held.remove(at);
            }
        });
    }

    pub(super) fn assert_free(key: usize) {
        HELD.with(|held| {
            assert!(
                !held.borrow().contains(&key),
                "status waiters woken with the cell locked"
            );
        });
    }

    pub(super) fn after_unchanged_read() {
        if let Some(hook) = AFTER_UNCHANGED_READ.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    /// Installs `hook` to run in the window of the next wait on this thread.
    pub(crate) fn set_after_unchanged_read(hook: impl FnOnce() + 'static) {
        AFTER_UNCHANGED_READ.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    /// A waker that counts its wakes.
    #[derive(Default)]
    pub(crate) struct CountWake(std::sync::atomic::AtomicUsize);

    impl CountWake {
        pub(crate) fn count(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl std::task::Wake for CountWake {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::probe::{self, CountWake};
    use super::*;
    use crate::types::StopReason;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    fn plane(status: ActorStatus) -> (StatusTx, StatusRx) {
        StatusTx::channel(StatusValue::new(status))
    }

    fn poll_once<F: Future>(fut: &mut Pin<Box<F>>, wake: &Arc<CountWake>) -> Poll<F::Output> {
        let waker = Waker::from(wake.clone());
        let mut cx = Context::from_waker(&waker);
        fut.as_mut().poll(&mut cx)
    }

    /// A waker that, when it runs, records whether the plane's lock was
    /// free for the thread that woke it.
    struct LockProbe {
        record: AbortRecord,
        free: std::sync::atomic::AtomicUsize,
        locked: std::sync::atomic::AtomicUsize,
        /// The version and the closed bit at each wake.
        states: Mutex<Vec<(u64, bool)>>,
    }

    impl LockProbe {
        fn new(record: AbortRecord) -> Arc<Self> {
            Arc::new(Self {
                record,
                free: Default::default(),
                locked: Default::default(),
                states: Mutex::new(Vec::new()),
            })
        }

        fn note(&self) {
            let counter = match self.record.status_cell().value.try_lock() {
                Ok(_) | Err(std::sync::TryLockError::Poisoned(_)) => &self.free,
                Err(std::sync::TryLockError::WouldBlock) => &self.locked,
            };
            counter.fetch_add(1, Ordering::SeqCst);
            self.states
                .lock()
                .unwrap()
                .push(self.record.status_cell().load());
        }

        /// The last wake found the version `version` or later and the
        /// plane closed as `closed`.
        fn last_wake_saw(&self) -> (u64, bool) {
            *self.states.lock().unwrap().last().expect("a wake ran")
        }

        fn free(&self) -> usize {
            self.free.load(Ordering::SeqCst)
        }

        fn locked(&self) -> usize {
            self.locked.load(Ordering::SeqCst)
        }
    }

    impl Wake for LockProbe {
        fn wake(self: Arc<Self>) {
            self.note();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.note();
        }
    }

    /// A waker that counts its wakes and panics in them while it is armed.
    #[derive(Default)]
    struct LoudWake {
        armed: std::sync::atomic::AtomicBool,
        woken: std::sync::atomic::AtomicUsize,
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

    /// A waiter's waker belongs to the waiter's task. When the write that
    /// wakes it is the actor's own publish and the wake panics, the panic stays
    /// out of the write: the value is published, the write reports that it
    /// changed it, and the waiter's next poll sees the new version.
    #[test]
    fn a_write_whose_waiter_waker_panics_still_publishes_and_returns() {
        let (tx, mut rx) = plane(ActorStatus::Running);
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut cx = Context::from_waker(&waker);
        let mut changed = Box::pin(rx.changed());
        assert!(changed.as_mut().poll(&mut cx).is_pending());
        loud.arm(true);
        let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tx.send_if_modified(|value| {
                value.status = ActorStatus::Stopping;
                true
            })
        }));
        loud.arm(false);
        assert_eq!(
            sent.ok(),
            Some(true),
            "the waiter's waker unwound out of the write"
        );
        assert_eq!(loud.woken(), 1, "the waiter's waker was woken once");
        assert_eq!(tx.status(), ActorStatus::Stopping);
        assert!(matches!(
            changed.as_mut().poll(&mut cx),
            Poll::Ready(Ok(()))
        ));
    }

    /// The close of the plane wakes the waiters the same way, from the drop of
    /// the last sender: a panic of a waiter's waker stays inside that drop, and
    /// the plane reads closed.
    #[test]
    fn a_close_whose_waiter_waker_panics_does_not_unwind_out_of_the_senders_drop() {
        let (tx, mut rx) = plane(ActorStatus::Running);
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut cx = Context::from_waker(&waker);
        let mut changed = Box::pin(rx.changed());
        assert!(changed.as_mut().poll(&mut cx).is_pending());
        loud.arm(true);
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(tx)));
        loud.arm(false);
        assert!(
            dropped.is_ok(),
            "the waiter's waker unwound out of the drop"
        );
        assert_eq!(loud.woken(), 1, "the waiter's waker was woken once");
        assert!(matches!(
            changed.as_mut().poll(&mut cx),
            Poll::Ready(Err(RecvError))
        ));
    }

    /// The last sender is dropped by a task that is unwinding (an actor's task
    /// that panicked outside every catch), and a waiter's waker panics in the
    /// drop's wake. A panic that escaped the drop would be a second panic
    /// during an unwind and end the process; contained, the unwind goes on to
    /// its end and the plane reads closed.
    #[test]
    fn a_close_during_an_unwind_whose_waiter_waker_panics_does_not_end_the_process() {
        if !crate::actor::panic::in_a_child_process(
            module_path!(),
            "a_close_during_an_unwind_whose_waiter_waker_panics_does_not_end_the_process",
        ) {
            return;
        }
        let (tx, mut rx) = plane(ActorStatus::Running);
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        let mut cx = Context::from_waker(&waker);
        let mut changed = Box::pin(rx.changed());
        assert!(changed.as_mut().poll(&mut cx).is_pending());
        loud.arm(true);
        let unwound = std::thread::spawn(move || {
            let _owned = tx;
            panic!("the task unwinds");
        })
        .join();
        loud.arm(false);
        assert!(unwound.is_err(), "the thread ended by its own panic");
        assert_eq!(loud.woken(), 1, "the waiter's waker was woken once");
        assert!(matches!(
            changed.as_mut().poll(&mut cx),
            Poll::Ready(Err(RecvError))
        ));
    }

    fn is_send<T: Send>(_: &T) {}

    fn assert_thread_safe<T: Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}

    struct Idle;

    impl crate::Actor for Idle {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::ActorContext<Self>,
        ) -> crate::ActorResult<()> {
            Ok(())
        }
    }

    #[test]
    fn an_actor_handle_stays_send_sync_and_unwind_safe() {
        fn check<T: Send + Sync + std::panic::UnwindSafe>() {}
        check::<crate::ActorHandle<Idle>>();
    }

    #[test]
    fn the_plane_types_and_their_waits_cross_threads() {
        assert_thread_safe::<StatusTx>();
        assert_thread_safe::<StatusRx>();
        let (_tx, mut rx) = plane(ActorStatus::Running);
        is_send(&rx.changed());
        is_send(&rx.wait_for(|v| v.status == ActorStatus::Stopped));
    }

    #[test]
    fn a_new_plane_holds_its_value_with_nothing_unseen() {
        let (tx, rx) = plane(ActorStatus::Running);
        assert_eq!(rx.has_changed(), Ok(false));
        assert_eq!(rx.borrow().status, ActorStatus::Running);
        assert_eq!(rx.status(), ActorStatus::Running);
        assert_eq!(tx.status(), ActorStatus::Running);
        let (tx, rx) = AbortRecord::new().status_channel();
        assert_eq!(rx.status(), ActorStatus::Initializing);
        assert_eq!(tx.borrow().reason, None);
    }

    #[test]
    fn a_closed_plane_with_an_unseen_version_fails_has_changed_and_answers_changed_first() {
        let (tx, mut rx) = plane(ActorStatus::Running);
        tx.send_modify(|v| v.status = ActorStatus::Stopping);
        drop(tx);
        // Closed first for the test of the state, version first for the wait.
        assert_eq!(rx.has_changed(), Err(RecvError));
        let wake = Arc::new(CountWake::default());
        let mut fut = Box::pin(rx.changed());
        assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Ok(())));
        drop(fut);
        assert_eq!(rx.has_changed(), Err(RecvError));
        let mut fut = Box::pin(rx.changed());
        assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Err(RecvError)));
        drop(fut);
        assert_eq!(rx.borrow().status, ActorStatus::Stopping);
    }

    #[test]
    fn a_borrow_never_marks_a_version_seen() {
        let (tx, rx) = plane(ActorStatus::Running);
        tx.send_modify(|v| v.status = ActorStatus::Stopping);
        assert_eq!(rx.borrow().status, ActorStatus::Stopping);
        assert_eq!(rx.has_changed(), Ok(true));
        assert_eq!(tx.borrow().status, ActorStatus::Stopping);
        assert_eq!(rx.has_changed(), Ok(true));
    }

    #[test]
    fn a_clone_keeps_what_its_original_has_and_has_not_seen() {
        let (tx, mut rx) = plane(ActorStatus::Running);
        tx.send_modify(|v| v.status = ActorStatus::Stopping);
        let unseen = rx.clone();
        assert_eq!(
            unseen.has_changed(),
            Ok(true),
            "a clone of an unseen receiver"
        );
        let wake = Arc::new(CountWake::default());
        let mut fut = Box::pin(rx.changed());
        assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Ok(())));
        drop(fut);
        let seen = rx.clone();
        assert_eq!(seen.has_changed(), Ok(false), "a clone of a seen receiver");
        assert_eq!(
            unseen.has_changed(),
            Ok(true),
            "the earlier clone is unmoved"
        );
        assert!(seen.same_channel(&unseen));
    }

    #[test]
    fn two_planes_are_two_channels() {
        let (a_tx, a_rx) = plane(ActorStatus::Running);
        let (_b_tx, b_rx) = plane(ActorStatus::Running);
        assert!(a_rx.same_channel(&a_rx.clone()));
        assert!(a_tx.same_channel(&a_rx));
        assert!(!a_rx.same_channel(&b_rx));
        assert!(!a_tx.same_channel(&b_rx));
    }

    #[test]
    fn every_writer_moves_the_version_and_the_status_byte() {
        let (tx, rx) = plane(ActorStatus::Initializing);
        let agree = |what: &str| {
            assert_eq!(rx.status(), rx.borrow().status, "{what}: the receiver");
            assert_eq!(tx.status(), tx.borrow().status, "{what}: the sender");
        };
        let mut seen = rx.clone();
        let wake = Arc::new(CountWake::default());
        let mut moved = |what: &str, moved: bool| {
            let unseen = seen.has_changed() == Ok(true);
            assert_eq!(unseen, moved, "{what}: version");
            let mut fut = Box::pin(seen.changed());
            if moved {
                assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Ok(())), "{what}");
            } else {
                assert_eq!(poll_once(&mut fut, &wake), Poll::Pending, "{what}");
            }
        };
        agree("start");
        assert!(tx.send_if_modified(|v| {
            v.status = ActorStatus::Running;
            true
        }));
        agree("send_if_modified");
        moved("send_if_modified", true);
        // A closure that writes and declines moves no version, and the byte
        // still reads what the value holds.
        assert!(!tx.send_if_modified(|v| {
            v.status = ActorStatus::Stopping;
            false
        }));
        agree("declined write");
        moved("declined write", false);
        tx.send_modify(|v| v.status = ActorStatus::Running);
        agree("send_modify");
        moved("send_modify", true);
        let old = tx.send_replace(StatusValue {
            status: ActorStatus::Stopped,
            reason: Some(StopReason::Kill),
            final_at: None,
        });
        assert_eq!(old.status, ActorStatus::Running);
        agree("send_replace");
        moved("send_replace", true);
        assert_eq!(rx.status(), ActorStatus::Stopped);
        assert!(!StatusValue::write_reason(&tx, StopReason::Graceful, false));
        assert_eq!(rx.borrow().reason, Some(StopReason::Kill), "written once");
        moved("a second reason", false);
    }

    /// Parks a `changed` and a `wait_for` on the plane, runs `write`, and
    /// finds both woken (with the lock free for the waker) and both ready.
    fn parked_waiters_are_woken_by(write: impl FnOnce(&StatusTx)) {
        let (tx, rx) = plane(ActorStatus::Initializing);
        let probe = LockProbe::new(tx.record.clone());
        let counted = Arc::new(CountWake::default());
        let mut changed_rx = rx.clone();
        let mut wait_rx = rx.clone();
        let mut changed = Box::pin(changed_rx.changed());
        let mut wait = Box::pin(
            wait_rx.wait_for(|v| v.status != ActorStatus::Initializing || v.reason.is_some()),
        );
        let waker = Waker::from(probe.clone());
        assert!(changed
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        assert!(poll_once(&mut wait, &counted).is_pending());
        write(&tx);
        assert!(probe.free() >= 1, "the parked changed was woken");
        assert_eq!(probe.locked(), 0, "no wake ran with the plane's lock held");
        assert_eq!(
            probe.last_wake_saw(),
            (1, false),
            "the version was moved before the waiters were woken"
        );
        assert!(counted.count() >= 1, "the parked wait_for was woken");
        assert_eq!(
            changed.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Ready(Ok(()))
        );
        assert_eq!(poll_once(&mut wait, &counted), Poll::Ready(Ok(())));
    }

    #[test]
    fn a_parked_wait_wakes_on_send_if_modified() {
        parked_waiters_are_woken_by(|tx| {
            tx.send_if_modified(|v| {
                v.status = ActorStatus::Running;
                true
            });
        });
    }

    #[test]
    fn a_parked_wait_wakes_on_send_modify() {
        parked_waiters_are_woken_by(|tx| tx.send_modify(|v| v.status = ActorStatus::Running));
    }

    #[test]
    fn a_parked_wait_wakes_on_send_replace() {
        parked_waiters_are_woken_by(|tx| {
            tx.send_replace(StatusValue::new(ActorStatus::Running));
        });
    }

    #[test]
    fn a_parked_wait_wakes_on_write_reason() {
        parked_waiters_are_woken_by(|tx| {
            assert!(StatusValue::write_reason(tx, StopReason::Kill, false));
        });
    }

    #[test]
    fn a_parked_wait_wakes_on_write_reason_with_stopped() {
        parked_waiters_are_woken_by(|tx| {
            assert!(StatusValue::write_reason(tx, StopReason::Graceful, true));
        });
    }

    #[test]
    fn a_parked_wait_wakes_on_publish_stopped() {
        parked_waiters_are_woken_by(crate::actor::runtime::publish_stopped);
    }

    #[test]
    fn a_parked_wait_wakes_when_the_last_sender_drops() {
        let (tx, rx) = plane(ActorStatus::Running);
        let probe = LockProbe::new(tx.record.clone());
        let counted = Arc::new(CountWake::default());
        let mut changed_rx = rx.clone();
        let mut wait_rx = rx.clone();
        let mut changed = Box::pin(changed_rx.changed());
        let mut wait = Box::pin(wait_rx.wait_for(|v| v.status == ActorStatus::Stopped));
        let waker = Waker::from(probe.clone());
        assert!(changed
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        assert!(poll_once(&mut wait, &counted).is_pending());
        drop(tx);
        assert!(probe.free() >= 1, "the parked changed was woken");
        assert_eq!(probe.locked(), 0, "no wake ran with the plane's lock held");
        assert!(
            probe.last_wake_saw().1,
            "the plane was marked closed before the waiters were woken"
        );
        assert!(counted.count() >= 1, "the parked wait_for was woken");
        assert_eq!(
            changed.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Ready(Err(RecvError))
        );
        assert_eq!(poll_once(&mut wait, &counted), Poll::Ready(Err(RecvError)));
    }

    #[test]
    fn the_plane_closes_with_its_last_sender_and_not_before() {
        let (tx, rx) = plane(ActorStatus::Running);
        let second = tx.clone();
        let third = second.clone();
        let wake = Arc::new(CountWake::default());
        let mut waiter = rx.clone();
        let mut fut = Box::pin(waiter.changed());
        assert!(poll_once(&mut fut, &wake).is_pending());
        drop(tx);
        drop(second);
        assert_eq!(rx.has_changed(), Ok(false), "a sender is still alive");
        assert_eq!(wake.count(), 0, "nothing woke the waiter");
        assert!(poll_once(&mut fut, &wake).is_pending());
        drop(rx.clone());
        assert_eq!(
            rx.has_changed(),
            Ok(false),
            "a receiver's drop counts nothing"
        );
        drop(third);
        assert_eq!(rx.has_changed(), Err(RecvError));
        assert!(wake.count() >= 1);
        assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Err(RecvError)));
    }

    #[test]
    fn a_subscriber_keeps_no_sender_alive() {
        let (tx, rx) = plane(ActorStatus::Running);
        let extra = tx.subscribe();
        drop(tx);
        assert_eq!(rx.has_changed(), Err(RecvError));
        assert_eq!(extra.has_changed(), Err(RecvError));
    }

    #[test]
    fn wait_for_tests_the_current_value_first_and_the_closed_value_once() {
        let calls = std::cell::Cell::new(0usize);
        let wake = Arc::new(CountWake::default());
        // Open and seen, accepted: ready on the first poll.
        let (tx, mut rx) = plane(ActorStatus::Running);
        {
            let mut fut = Box::pin(rx.wait_for(|v| {
                calls.set(calls.get() + 1);
                v.status == ActorStatus::Running
            }));
            assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Ok(())));
        }
        assert_eq!(calls.replace(0), 1);
        // Open, declined, then written: two tests.
        {
            let mut fut = Box::pin(rx.wait_for(|v| {
                calls.set(calls.get() + 1);
                v.status == ActorStatus::Stopped
            }));
            assert!(poll_once(&mut fut, &wake).is_pending());
            tx.send_modify(|v| v.status = ActorStatus::Stopped);
            assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Ok(())));
        }
        assert_eq!(calls.replace(0), 2);
        drop(tx);
        // Closed and seen, declined: tested once, then Err.
        {
            let mut fut = Box::pin(rx.wait_for(|v| {
                calls.set(calls.get() + 1);
                v.status == ActorStatus::Running
            }));
            assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Err(RecvError)));
        }
        assert_eq!(calls.replace(0), 1);
        // Closed and seen, accepted: Ok.
        {
            let mut fut = Box::pin(rx.wait_for(|v| {
                calls.set(calls.get() + 1);
                v.status == ActorStatus::Stopped
            }));
            assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Ok(())));
        }
        assert_eq!(calls.replace(0), 1);
        // Closed with an unseen version: tested once on that version, and
        // the version is seen afterwards.
        let (tx, mut rx) = plane(ActorStatus::Running);
        tx.send_modify(|v| v.status = ActorStatus::Stopping);
        drop(tx);
        {
            let mut fut = Box::pin(rx.wait_for(|v| {
                calls.set(calls.get() + 1);
                v.status == ActorStatus::Stopped
            }));
            assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Err(RecvError)));
        }
        assert_eq!(calls.replace(0), 1);
        let (tx, mut rx) = plane(ActorStatus::Running);
        tx.send_modify(|v| v.status = ActorStatus::Stopped);
        drop(tx);
        {
            let mut fut = Box::pin(rx.wait_for(|v| {
                calls.set(calls.get() + 1);
                v.status == ActorStatus::Stopped
            }));
            assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Ok(())));
        }
        assert_eq!(calls.replace(0), 1);
    }

    #[test]
    fn wait_for_marks_the_version_it_tested_as_seen() {
        let (tx, mut rx) = plane(ActorStatus::Running);
        tx.send_modify(|v| v.status = ActorStatus::Stopping);
        assert_eq!(rx.has_changed(), Ok(true));
        let wake = Arc::new(CountWake::default());
        {
            let mut fut = Box::pin(rx.wait_for(|v| v.status == ActorStatus::Stopping));
            assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Ok(())));
        }
        assert_eq!(rx.has_changed(), Ok(false));
    }

    #[test]
    fn a_write_between_the_read_and_the_first_poll_still_wakes_changed() {
        let (tx, rx) = plane(ActorStatus::Initializing);
        let mut waiter = rx.clone();
        let writer = tx.clone();
        probe::set_after_unchanged_read(move || {
            writer.send_modify(|v| v.status = ActorStatus::Running);
        });
        let wake = Arc::new(CountWake::default());
        let mut fut = Box::pin(waiter.changed());
        assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Ok(())));
    }

    #[test]
    fn a_write_between_the_read_and_the_first_poll_still_wakes_wait_for() {
        let (tx, rx) = plane(ActorStatus::Initializing);
        let mut waiter = rx.clone();
        let writer = tx.clone();
        probe::set_after_unchanged_read(move || {
            writer.send_modify(|v| v.status = ActorStatus::Running);
        });
        let wake = Arc::new(CountWake::default());
        let mut fut = Box::pin(waiter.wait_for(|v| v.status == ActorStatus::Running));
        assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Ok(())));
    }

    #[test]
    fn the_close_between_the_read_and_the_first_poll_still_wakes_changed() {
        let (tx, rx) = plane(ActorStatus::Initializing);
        let mut waiter = rx.clone();
        let cell = std::cell::RefCell::new(Some(tx));
        let cell = Arc::new(std::sync::Mutex::new(cell.into_inner()));
        let closer = cell.clone();
        probe::set_after_unchanged_read(move || {
            drop(closer.lock().unwrap().take());
        });
        let wake = Arc::new(CountWake::default());
        let mut fut = Box::pin(waiter.changed());
        assert_eq!(poll_once(&mut fut, &wake), Poll::Ready(Err(RecvError)));
    }

    #[test]
    #[should_panic(expected = "locked twice on one thread")]
    fn a_second_lock_of_one_plane_on_one_thread_panics_instead_of_hanging() {
        let (tx, rx) = plane(ActorStatus::Running);
        let _first = rx.borrow();
        let _second = tx.borrow();
    }

    #[test]
    fn locks_of_two_planes_on_one_thread_do_not_trip_the_check() {
        let (_a_tx, a_rx) = plane(ActorStatus::Running);
        let (_b_tx, b_rx) = plane(ActorStatus::Stopped);
        let a = a_rx.borrow();
        let b = b_rx.borrow();
        assert_ne!(a.status, b.status);
    }

    #[test]
    #[should_panic(expected = "woken with the cell locked")]
    fn a_wake_with_the_lock_held_panics() {
        let (tx, rx) = plane(ActorStatus::Running);
        let _held = rx.borrow();
        tx.record.wake_status_waiters();
    }

    #[test]
    fn a_poisoned_lock_is_ignored() {
        let (tx, rx) = plane(ActorStatus::Running);
        let held = tx.clone();
        let panicked = std::thread::spawn(move || {
            let _guard = held.record.status_cell().value.lock().unwrap();
            panic!("a panic while the plane is locked");
        })
        .join();
        assert!(panicked.is_err());
        assert_eq!(rx.borrow().status, ActorStatus::Running);
        tx.send_modify(|v| v.status = ActorStatus::Stopped);
        assert_eq!(rx.status(), ActorStatus::Stopped);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_writers_never_leave_a_waiter_asleep() {
        for iteration in 0..200 {
            let (tx, rx) = plane(ActorStatus::Initializing);
            let mut waiters = Vec::new();
            for _ in 0..6 {
                let mut waiter = rx.clone();
                waiters.push(tokio::spawn(async move {
                    waiter
                        .wait_for(|v| v.status == ActorStatus::Stopped)
                        .await
                        .is_ok()
                }));
                let mut watcher = rx.clone();
                waiters.push(tokio::spawn(async move {
                    while watcher.changed().await.is_ok() {}
                    watcher.status() == ActorStatus::Stopped
                }));
            }
            let writer = tokio::spawn(async move {
                for n in 0..5 {
                    tx.send_modify(|v| {
                        v.status = if n % 2 == 0 {
                            ActorStatus::Running
                        } else {
                            ActorStatus::Stopping
                        }
                    });
                    tokio::task::yield_now().await;
                }
                crate::actor::runtime::publish_stopped(&tx);
            });
            writer.await.unwrap();
            for waiter in waiters {
                let done = tokio::time::timeout(std::time::Duration::from_secs(10), waiter)
                    .await
                    .unwrap_or_else(|_| panic!("iteration {iteration}: a waiter was never woken"));
                assert!(done.unwrap(), "iteration {iteration}");
            }
        }
    }

    /// A reader that consumed a version through `changed` reads, at once
    /// and without the lock, the status byte of that version or a later one.
    /// The iterations are bounded by their count and by the time they may take,
    /// so a host that starves the threads ends the run early instead of
    /// stretching it.
    #[test]
    fn a_waiter_that_consumed_a_version_reads_its_status() {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        let violations = Arc::new(AtomicUsize::new(0));
        let iterations = 200_000usize;
        let budget = std::time::Duration::from_secs(20);
        let more = Arc::new(AtomicBool::new(true));
        let readers = 3usize;
        let barrier = Arc::new(std::sync::Barrier::new(readers + 1));
        let planes: Arc<Mutex<Option<(StatusTx, StatusRx)>>> = Arc::new(Mutex::new(None));
        let mut joins = Vec::new();
        for _ in 0..readers {
            let (barrier, planes, violations, more) = (
                barrier.clone(),
                planes.clone(),
                violations.clone(),
                more.clone(),
            );
            joins.push(std::thread::spawn(move || {
                let wake = Arc::new(CountWake::default());
                let waker = Waker::from(wake);
                loop {
                    barrier.wait();
                    if !more.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut rx = planes.lock().unwrap().as_ref().unwrap().1.clone();
                    loop {
                        let mut cx = Context::from_waker(&waker);
                        let mut fut = Box::pin(rx.changed());
                        if let Poll::Ready(Ok(())) = fut.as_mut().poll(&mut cx) {
                            drop(fut);
                            if rx.status() != ActorStatus::Stopped {
                                violations.fetch_add(1, Ordering::SeqCst);
                            }
                            break;
                        }
                        drop(fut);
                        std::hint::spin_loop();
                    }
                    barrier.wait();
                }
            }));
        }
        let began = std::time::Instant::now();
        for step in 0..iterations {
            if step % 64 == 0 && began.elapsed() >= budget {
                break;
            }
            *planes.lock().unwrap() = Some(plane(ActorStatus::Running));
            barrier.wait();
            for _ in 0..(iterations % 7) {
                std::hint::spin_loop();
            }
            let tx = planes.lock().unwrap().as_ref().unwrap().0.clone();
            crate::actor::runtime::publish_stopped(&tx);
            barrier.wait();
        }
        more.store(false, Ordering::SeqCst);
        barrier.wait();
        for j in joins {
            j.join().unwrap();
        }
        assert_eq!(
            violations.load(Ordering::SeqCst),
            0,
            "stale status byte after a consumed version"
        );
    }
}
