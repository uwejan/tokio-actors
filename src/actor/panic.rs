//! Poll-level panic capture for actor callbacks.
//!
//! Panics inside actor callbacks are caught at the callback boundary and
//! converted into [`ActorError::Panic`](crate::error::ActorError::Panic), so a
//! crash becomes a visible `StopReason::Failure` instead of silently unwinding
//! the actor task. This mirrors gen_server, which catches callback exceptions,
//! runs `terminate/2`, and exits with the original reason.
//!
//! A panic in a hook is a panic of the hook wherever it is raised: in the call
//! that builds the hook's future, in a poll of the future, or in dropping the
//! future once it has completed. That panic is the hook's result, whatever the
//! hook had produced. The actor is then handed no further message and is
//! observed only by the hooks of its stop (the final deliveries of a supervisor
//! and `on_stopped`; none after a panic of `pre_start` or `on_started`), and
//! then dropped. Two values whose drops both panic in one frame abort the
//! process, as in any Rust code.
//!
//! Unwind-safety note: the callback future borrows `&mut` actor state, so the
//! catch uses `AssertUnwindSafe`. This is sound (no `unsafe` involved; a panic
//! cannot cause memory unsafety) and bounded by the rule above. Tokio's own
//! task harness wraps every task future in `AssertUnwindSafe` the same way.

use std::any::Any;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::sync::oneshot;

use crate::types::ActorId;

/// Opaque panic payload as produced by the unwind machinery.
pub(crate) type PanicPayload = Box<dyn Any + Send + 'static>;

/// Awaits `fut`, catching an unwinding panic from any of its polls.
///
/// For a future the crate built itself, which holds no message of an actor;
/// the hooks of an actor go through [`catch_hook`]. Returns `Ok(output)` on
/// normal completion or `Err(payload)` if a poll panicked. Zero heap
/// allocation: the future is stack-pinned. However the awaiting future is
/// dropped (an abort while `fut` is suspended included), `fut` and everything
/// it holds drop under `catch_unwind`, so that drop never unwinds.
pub(crate) async fn catch_callback<F: Future>(fut: F) -> Result<F::Output, PanicPayload> {
    let slot = Some(fut);
    tokio::pin!(slot);
    let mut slot = DropCaught(slot);
    std::future::poll_fn(|cx| {
        let fut = slot
            .0
            .as_mut()
            .as_pin_mut()
            .expect("polled only until it completes");
        match catch_unwind(AssertUnwindSafe(|| fut.poll(cx))) {
            Ok(poll) => poll.map(Ok),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    })
    .await
}

/// A pinned slot whose value is dropped in place under `catch_unwind` when
/// the slot's guard is dropped; a panic that drop raises is discarded.
struct DropCaught<'a, F>(Pin<&'a mut Option<F>>);

impl<F> Drop for DropCaught<'_, F> {
    fn drop(&mut self) {
        let slot = &mut self.0;
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| slot.set(None))) {
            discard(payload);
        }
    }
}

/// Calls `build` and awaits the future it returns, catching an unwinding
/// panic from the call, from any poll and from the drop of the completed
/// future.
///
/// For the hooks of an actor. A hook written as a plain `fn` that returns a
/// future runs code before its `async` block; that code runs in the first
/// poll, inside a catch, like everything the future does afterwards. Returns
/// `Ok(output)` on normal completion or `Err(payload)` if the call, a poll or
/// the drop of the completed future panicked. Zero heap allocation: the
/// future is moved into its stack-pinned slot before its first poll and
/// polled there. The poll that completes the future drops it inside the
/// catch that polled it; a drop that panics after the output is ready is a
/// panic of the hook, so it is the result and the output is dropped under a
/// catch. However the returned future is dropped (before its first poll, or
/// while the callback is suspended, an abort included), the call that was to
/// build the future, or the future and everything it holds, the message the
/// hook handles included, drop under `catch_unwind`, so that drop never
/// unwinds. A future the crate built itself goes through [`catch_callback`],
/// which holds it once.
pub(crate) fn catch_hook<B, F>(build: B) -> impl Future<Output = Result<F::Output, PanicPayload>>
where
    B: FnOnce() -> F,
    F: Future,
{
    let mut unbuilt = Unbuilt(Some(build));
    async move {
        let slot: Option<F> = None;
        tokio::pin!(slot);
        let mut running = Running(slot);
        // The call runs here, once, in the first resume and ahead of the poll
        // loop. The future it returns is moved into its pinned slot before its
        // first poll; until then only the call is held.
        if let Some(build) = unbuilt.0.take() {
            catch_unwind(AssertUnwindSafe(|| running.0.set(Some(build()))))?;
        }
        PollStep {
            running: &mut running,
        }
        .await
    }
}

/// One poll of a hook's future, and the drop of the future when that poll
/// completes it, under one `catch_unwind`. A panic in the poll is the result;
/// so is a panic in the drop of a completed future, and the output the hook
/// produced is then dropped under a catch. `poll` carries `#[inline(always)]`
/// so that the step is compiled into the block that awaits it and is not
/// called from there.
struct PollStep<'a, 'b, F> {
    running: &'a mut Running<'b, F>,
}

impl<F: Future> Future for PollStep<'_, '_, F> {
    type Output = Result<F::Output, PanicPayload>;

    #[inline(always)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let slot = &mut self.get_mut().running.0;
        let mut output = None;
        let polled = catch_unwind(AssertUnwindSafe(|| {
            let poll = slot
                .as_mut()
                .as_pin_mut()
                .expect("polled only until it completes")
                .poll(cx);
            if let Poll::Ready(value) = poll {
                output = Some(value);
                slot.set(None);
            }
        }));
        match (output, polled) {
            (Some(value), Ok(())) => Poll::Ready(Ok(value)),
            (Some(value), Err(payload)) => {
                drop_caught(value);
                Poll::Ready(Err(payload))
            }
            (None, Ok(())) => Poll::Pending,
            (None, Err(payload)) => Poll::Ready(Err(payload)),
        }
    }
}

/// The call that builds a hook's future, until the first poll takes it. If it
/// is still held when the guard is dropped, it is moved out and dropped inside
/// `catch_unwind`; a panic that drop raises is discarded.
struct Unbuilt<B>(Option<B>);

impl<B> Drop for Unbuilt<B> {
    fn drop(&mut self) {
        if let Some(build) = self.0.take() {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(move || drop(build))) {
                discard(payload);
            }
        }
    }
}

/// The pinned slot of a hook's future. A future still in the slot when the
/// guard is dropped (the awaiting future was dropped while the hook was
/// suspended, or a poll panicked) is dropped in place under `catch_unwind`; a
/// panic that drop raises is discarded.
struct Running<'a, F>(Pin<&'a mut Option<F>>);

impl<F> Drop for Running<'_, F> {
    fn drop(&mut self) {
        if self.0.is_some() {
            let slot = &mut self.0;
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| slot.set(None))) {
                discard(payload);
            }
        }
    }
}

/// Runs a synchronous closure, catching an unwinding panic.
///
/// Used for factory closures during restarts, where a panicking factory must
/// surface as a restart failure instead of killing the restart task.
pub(crate) fn catch_sync<T>(f: impl FnOnce() -> T) -> Result<T, PanicPayload> {
    catch_unwind(AssertUnwindSafe(f))
}

/// Renders a panic payload as a human-readable string.
///
/// `panic!` payloads are `&'static str` or `String` in practice (even
/// `panic!("{}", x)` with a const-foldable argument yields `&'static str`);
/// anything else (`panic_any`) becomes a placeholder. The opaque payload is
/// dropped inside its own catch because a payload's `Drop` may itself panic.
pub(crate) fn payload_into_string(payload: PanicPayload) -> String {
    let payload = match payload.downcast::<&'static str>() {
        Ok(s) => return (*s).to_string(),
        Err(p) => p,
    };
    let payload = match payload.downcast::<String>() {
        Ok(s) => return *s,
        Err(p) => p,
    };
    discard(payload);
    "non-string panic payload".to_string()
}

/// Drops a panic payload inside a catch. A payload whose drop panics again
/// leaves a second payload, which is forgotten rather than dropped, so this
/// never unwinds.
pub(crate) fn discard(payload: PanicPayload) {
    if let Err(again) = catch_unwind(AssertUnwindSafe(move || drop(payload))) {
        std::mem::forget(again);
    }
}

/// Runs `work`, the wake or the drop of wakers that belong to other tasks (a
/// waker's `wake`, a `Notify`'s `notify_waiters`, a channel's send or close),
/// so that a panic of one of those wakers never unwinds into the code that
/// made the call.
///
/// A waker belongs to the task that polls with it. The code that wakes it on
/// that task's behalf (a stop's raise, an actor publishing its status, a drop
/// that closes a plane) runs for other work, and a waker's panic must change
/// none of it: no raised request, published status, reason or unit is lost,
/// the work after the call still runs, and a call made from a drop that runs
/// during an unwind adds no second panic to it. The panic is caught here, its
/// payload is dropped inside a catch (a payload's drop can panic too), and
/// with the `tracing` feature it is logged as a warning that names `site`
/// and, when it is known, the actor.
///
/// Where the crate owns the wait list, it wakes each waiter under its own
/// call, so a panic stops no other waiter's wake. Where tokio owns the batch
/// (a `Notify`, a `watch` channel), a panic ends tokio's wake of that batch:
/// the wakers it had taken and not yet woken are dropped without a wake, and
/// the waiters it had not yet reached are marked notified and left unwoken, so
/// their futures finish at their next poll and not before. Only a panicking
/// waker causes this, and the panic is contained. A list of tokio's that users'
/// wakers and the library's own waits would share is split in two, each woken
/// under its own catch, so that the waker of a user strands no wait of the
/// library: the observers of a subtree have a list of their own.
///
/// A waker the polling task itself replaces or drops in its own poll is not
/// the business of this function: that panic unwinds into that poll.
pub(crate) fn contain_waker_panic(
    site: &'static str,
    owner: Option<&ActorId>,
    work: impl FnOnce(),
) {
    if let Err(payload) = catch_sync(work) {
        log_waker_panic(site, &payload_into_string(payload), owner);
    }
}

/// Sends `value` on a one-shot channel whose receiver is another task's future
/// (the caller of a request, of a status query or of a spawn waits on it), so
/// that a panic of that task's waker never unwinds into the sender.
///
/// The send wakes the receiver's waker inside tokio's call, after the value is
/// stored, so a panic of the wake leaves the value delivered: it is contained
/// as [`contain_waker_panic`] contains it, and the send reports `Ok(())`. A
/// receiver that is gone gives the value back, as `send` does.
pub(crate) fn send_contained<T>(tx: oneshot::Sender<T>, value: T) -> Result<(), T> {
    match catch_unwind(AssertUnwindSafe(move || tx.send(value))) {
        Ok(sent) => sent,
        Err(payload) => {
            log_waker_panic("reply", &payload_into_string(payload), None);
            Ok(())
        }
    }
}

/// The sender of an answer that a frame of an actor's task holds until the
/// answer goes out, and whose receiver is another task's future: the start ack
/// the caller of a spawn waits on, the responder of a request, the answer held
/// for a request whose handler failed.
///
/// A sender that is dropped without a send wakes its receiver's waker inside
/// tokio's drop, and that waker belongs to the caller's task. Held in this
/// guard, the sender is released inside [`contain_waker_panic`] however the
/// guard goes. That matters when the task unwinds from a panic of its own: the
/// unwind drops everything its frames hold, and a second panic raised by a
/// drop in the middle of it ends the process. A send takes the sender out
/// first ([`take`](Self::take)) and goes through [`send_contained`], so the
/// guard then has nothing left to release. The guard is the size of an
/// `Option` of the sender.
pub(crate) struct AnswerHold<T>(Option<oneshot::Sender<T>>);

impl<T> AnswerHold<T> {
    /// Holds `answer`, when there is one.
    pub(crate) fn new(answer: Option<oneshot::Sender<T>>) -> Self {
        Self(answer)
    }

    /// Takes the sender out of the guard, which then holds nothing.
    pub(crate) fn take(&mut self) -> Option<oneshot::Sender<T>> {
        self.0.take()
    }
}

impl<T> Drop for AnswerHold<T> {
    fn drop(&mut self) {
        if let Some(answer) = self.0.take() {
            contain_waker_panic("answer", None, move || drop(answer));
        }
    }
}

/// Logs, with the `tracing` feature, a panic raised by the wake or the drop of
/// a waker that [`contain_waker_panic`] contained, naming the actor when it is
/// known.
pub(crate) fn log_waker_panic(site: &'static str, payload: &str, owner: Option<&ActorId>) {
    #[cfg(feature = "tracing")]
    match owner {
        Some(owner) => caught_log!(warn,
            actor_id = %owner,
            site,
            panic = %payload,
            "a waker panicked when the crate woke or dropped it; the panic is contained"
        ),
        None => caught_log!(warn,
            site,
            panic = %payload,
            "a waker panicked when the crate woke or dropped it; the panic is contained"
        ),
    }
    #[cfg(not(feature = "tracing"))]
    let _ = (site, payload, owner);
}

/// Drops a value inside a catch; a panic that drop raises is discarded. Only a
/// panic has handed a value to this, so the call stays out of the step that
/// polls a hook.
#[cold]
#[inline(never)]
fn drop_caught<T>(value: T) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(move || drop(value))) {
        discard(payload);
    }
}

/// Runs one log call of the crate (see `caught_log!`), containing a panic of
/// the `tracing` subscriber, so a log line never unwinds into the code that
/// writes it. A payload that is a `&str` or a `String` is dropped, which
/// cannot panic. Any other payload is forgotten, never dropped: no drop glue
/// of the subscriber's runs here, at the cost of one leaked box per such
/// panic.
#[cfg(feature = "tracing")]
pub(crate) fn contain_log(log: impl FnOnce()) {
    if let Err(payload) = catch_sync(log) {
        if payload.is::<&'static str>() || payload.is::<String>() {
            drop(payload);
        } else {
            std::mem::forget(payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic;

    /// The text a [`DropBomb`] panics with.
    const HELD_PANIC: &str = "a held value panicked when dropped";

    /// The text a [`CountedBomb`] panics with.
    const COUNTED_PANIC: &str = "a counted value panicked when dropped";

    /// A future that is ready with `value` at its first poll and keeps `held`
    /// until it is dropped.
    struct ReadyHolding<T, V> {
        value: Option<T>,
        _held: V,
    }

    impl<T: Unpin, V: Unpin> Future for ReadyHolding<T, V> {
        type Output = T;

        fn poll(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<T> {
            Poll::Ready(self.get_mut().value.take().expect("polled once"))
        }
    }

    /// The output the hook produced is dropped once, and under a catch: its
    /// own drop panics too, and the result is still the panic of the
    /// future's drop.
    #[tokio::test]
    async fn the_output_of_a_hook_whose_drop_panics_is_dropped_once_under_a_catch() {
        let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let out = catch_hook(|| ReadyHolding {
            value: Some(CountedBomb(drops.clone())),
            _held: DropBomb,
        })
        .await;
        let payload = out.err().expect("the drop of the future panicked");
        assert_eq!(payload_into_string(payload), HELD_PANIC);
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// The panic of the output's drop is dropped under a catch too: a payload
    /// whose own drop panics does not unwind either.
    #[test]
    fn the_panic_of_the_output_of_a_hook_whose_drop_panics_is_dropped_under_a_catch() {
        let mut hook = Box::pin(catch_hook(|| ReadyHolding {
            value: Some(PayloadBomb),
            _held: DropBomb,
        }));
        let mut polled = None;
        assert!(
            !unwinds(|| polled = Some(poll_once(&mut hook))),
            "the drop of the output unwound out of the hook"
        );
        match polled {
            Some(Poll::Ready(Err(payload))) => {
                assert_eq!(payload_into_string(payload), HELD_PANIC)
            }
            Some(Poll::Ready(Ok(output))) => {
                // Dropping the output would panic with a payload that panics.
                std::mem::forget(output);
                panic!("the hook reported an output though the drop of its future panicked");
            }
            _ => panic!("the hook did not complete in one poll"),
        }
    }

    /// A value whose drop panics.
    struct DropBomb;

    impl Drop for DropBomb {
        fn drop(&mut self) {
            panic!("{HELD_PANIC}");
        }
    }

    /// A value whose drop counts itself and then panics.
    struct CountedBomb(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for CountedBomb {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("{COUNTED_PANIC}");
        }
    }

    /// A panic payload whose own drop panics.
    struct BadPayload;

    impl Drop for BadPayload {
        fn drop(&mut self) {
            panic!("the payload panicked when dropped");
        }
    }

    /// A value whose drop panics with a [`BadPayload`].
    struct PayloadBomb;

    impl Drop for PayloadBomb {
        fn drop(&mut self) {
            panic::panic_any(BadPayload);
        }
    }

    /// Runs `f` and reports whether it unwound. A payload that unwound is
    /// leaked, not dropped: dropping a [`BadPayload`] panics again, outside
    /// any catch, which would end the test's thread before it reports.
    fn unwinds(f: impl FnOnce()) -> bool {
        match catch_unwind(AssertUnwindSafe(f)) {
            Ok(()) => false,
            Err(payload) => {
                std::mem::forget(payload);
                true
            }
        }
    }

    /// Polls `fut` once, with a waker that does nothing.
    fn poll_once<F: Future + Unpin>(fut: &mut F) -> Poll<F::Output> {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        Pin::new(fut).poll(&mut cx)
    }

    /// The awaiting future of a callback future that holds `held` across a
    /// suspension, polled to that suspension.
    fn callback_suspended_with<V: Send + 'static>(held: V) -> Pin<Box<impl Future>> {
        let mut awaiting = Box::pin(catch_callback(async move {
            let _held = held;
            std::future::pending::<()>().await;
        }));
        assert!(poll_once(&mut awaiting).is_pending());
        awaiting
    }

    /// The awaiting future of a hook that holds `held` across a suspension,
    /// polled to that suspension.
    fn hook_suspended_with<V: Send + 'static>(held: V) -> Pin<Box<impl Future>> {
        let mut awaiting = Box::pin(catch_hook(move || async move {
            let _held = held;
            std::future::pending::<()>().await;
        }));
        assert!(poll_once(&mut awaiting).is_pending());
        awaiting
    }

    /// Drops the awaiting future of a hook before it was polled, the call that
    /// builds the future holding `held`, and reports whether the drop
    /// unwound.
    fn dropping_a_call_that_never_ran<V>(held: V) -> bool {
        let awaiting = Box::pin(catch_hook(move || {
            let _held = &held;
            std::future::ready(())
        }));
        unwinds(move || drop(awaiting))
    }

    /// The panic of such a drop is dropped under a catch too: a payload whose
    /// own drop panics does not unwind either.
    #[test]
    fn the_panic_of_a_drop_while_suspended_is_dropped_under_a_catch() {
        let awaiting = callback_suspended_with(PayloadBomb);
        assert!(
            !unwinds(move || drop(awaiting)),
            "the drop of the panic's payload unwound"
        );
    }

    /// The panic of such a drop is dropped under a catch too: a payload whose
    /// own drop panics does not unwind either.
    #[test]
    fn the_panic_of_a_hook_s_drop_while_suspended_is_dropped_under_a_catch() {
        let awaiting = hook_suspended_with(PayloadBomb);
        assert!(
            !unwinds(move || drop(awaiting)),
            "the drop of the panic's payload unwound"
        );
    }

    /// An awaiting future dropped before its first poll drops the call that
    /// was to build the future under a catch: a drop that panics does not
    /// unwind out of the drop of the awaiting future.
    #[test]
    fn a_hook_call_that_never_ran_is_dropped_under_a_catch() {
        assert!(
            !dropping_a_call_that_never_ran(DropBomb),
            "the drop of what the call held unwound"
        );
    }

    /// The call that was to build the future is dropped once, not leaked.
    #[test]
    fn a_hook_dropped_before_its_first_poll_drops_its_call_once() {
        let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        assert!(
            !dropping_a_call_that_never_ran(CountedBomb(drops.clone())),
            "the drop of what the call held unwound"
        );
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// The panic of such a drop is dropped under a catch too: a payload whose
    /// own drop panics does not unwind either.
    #[test]
    fn the_panic_of_a_call_that_never_ran_is_dropped_under_a_catch() {
        assert!(
            !dropping_a_call_that_never_ran(PayloadBomb),
            "the drop of the panic's payload unwound"
        );
    }

    /// The text a [`LoudWake`] panics with.
    const LOUD_PANIC: &str = "a waker's wake panics";

    /// A waker that counts its wakes and panics in them while it is armed.
    #[derive(Default)]
    struct LoudWake {
        armed: std::sync::atomic::AtomicBool,
        woken: std::sync::atomic::AtomicUsize,
    }

    impl std::task::Wake for LoudWake {
        fn wake(self: std::sync::Arc<Self>) {
            self.woken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
                panic!("{LOUD_PANIC}");
            }
        }
    }

    impl LoudWake {
        fn woken(&self) -> usize {
            self.woken.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// An answer held in a guard, with its receiver parked by a waker that
    /// panics when it is woken.
    fn held_answer_with_a_loud_receiver() -> (
        AnswerHold<u32>,
        Pin<Box<oneshot::Receiver<u32>>>,
        std::sync::Arc<LoudWake>,
    ) {
        let (tx, rx) = oneshot::channel();
        let loud = std::sync::Arc::new(LoudWake::default());
        let waker = std::task::Waker::from(loud.clone());
        let mut rx = Box::pin(rx);
        assert!(rx
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending());
        loud.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        (AnswerHold::new(Some(tx)), rx, loud)
    }
    /// Collects every WARN event as one line of `name=value` pairs.
    #[cfg(feature = "tracing")]
    #[derive(Clone, Default)]
    struct Warnings {
        lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[cfg(feature = "tracing")]
    impl tracing::Subscriber for Warnings {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }

        fn new_span(&self, _attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            struct Text(String);
            impl tracing::field::Visit for Text {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write as _;
                    let _ = write!(self.0, "{}={:?} ", field.name(), value);
                }
            }
            let mut text = Text(String::new());
            event.record(&mut text);
            self.lines.lock().unwrap().push(text.0);
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// A held answer dropped unanswered whose receiver's waker panics leaves
    /// one warning that names the site and carries the panic's text.
    #[cfg(feature = "tracing")]
    #[test]
    fn a_held_answer_whose_waker_panics_is_logged_once() {
        let warnings = Warnings::default();
        let lines = warnings.lines.clone();
        tracing::subscriber::with_default(warnings, || {
            let (hold, _rx, loud) = held_answer_with_a_loud_receiver();
            drop(hold);
            assert_eq!(loud.woken(), 1, "the drop woke the receiver");
        });
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1, "one warning for the one contained panic");
        assert!(
            lines[0].contains("site=\"answer\"") && lines[0].contains(LOUD_PANIC),
            "the warning names the site and the panic: {}",
            lines[0]
        );
    }
}
