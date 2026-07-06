//! Poll-level panic capture for actor callbacks.
//!
//! Panics inside actor callbacks are caught at the callback boundary and
//! converted into [`ActorError::Panic`](crate::error::ActorError::Panic), so a
//! crash becomes a visible `StopReason::Failure` instead of silently unwinding
//! the actor task. This mirrors gen_server, which catches callback exceptions,
//! runs `terminate/2`, and exits with the original reason.
//!
//! Unwind-safety note: the callback future borrows `&mut` actor state, so the
//! catch uses `AssertUnwindSafe`. This is sound (no `unsafe` involved; a panic
//! cannot cause memory unsafety) and bounded: after a caught panic the actor is
//! only observed by `on_stopped` and then dropped, never handed another message.
//! Tokio's own task harness wraps every task future in `AssertUnwindSafe` the
//! same way.

use std::any::Any;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::task::Poll;

/// Opaque panic payload as produced by the unwind machinery.
pub(crate) type PanicPayload = Box<dyn Any + Send + 'static>;

/// Awaits `fut`, catching an unwinding panic from any of its polls.
///
/// Returns `Ok(output)` on normal completion or `Err(payload)` if a poll
/// panicked. Zero heap allocation: the future is stack-pinned.
pub(crate) async fn catch_callback<F: Future>(fut: F) -> Result<F::Output, PanicPayload> {
    tokio::pin!(fut);
    std::future::poll_fn(
        |cx| match catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(poll) => poll.map(Ok),
            Err(payload) => Poll::Ready(Err(payload)),
        },
    )
    .await
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
    let _ = catch_unwind(AssertUnwindSafe(move || drop(payload)));
    "non-string panic payload".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic;

    #[tokio::test]
    async fn passes_through_normal_completion() {
        let out = catch_callback(async { 41 + 1 }).await;
        assert_eq!(out.unwrap(), 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catches_panic_before_first_await() {
        let out = catch_callback(async {
            panic!("early boom");
            #[allow(unreachable_code)]
            ()
        })
        .await;
        let payload = out.unwrap_err();
        assert_eq!(payload_into_string(payload), "early boom");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catches_panic_after_await_point() {
        let out = catch_callback(async {
            tokio::task::yield_now().await;
            panic!("late boom");
            #[allow(unreachable_code)]
            ()
        })
        .await;
        let payload = out.unwrap_err();
        assert_eq!(payload_into_string(payload), "late boom");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn formatted_runtime_payload_is_string() {
        let value = std::hint::black_box(7);
        let out = catch_callback(async move {
            panic!("value was {value}");
        })
        .await;
        assert_eq!(payload_into_string(out.unwrap_err()), "value was 7");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_string_payload_becomes_placeholder() {
        let out = catch_callback(async {
            panic::panic_any(1234_u32);
        })
        .await;
        assert_eq!(
            payload_into_string(out.unwrap_err()),
            "non-string panic payload"
        );
    }

    #[test]
    fn catch_sync_catches() {
        let err = catch_sync(|| -> u32 { panic!("sync boom") }).unwrap_err();
        assert_eq!(payload_into_string(err), "sync boom");
        assert_eq!(catch_sync(|| 5).unwrap(), 5);
    }
}
