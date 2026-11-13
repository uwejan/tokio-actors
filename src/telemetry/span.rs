//! The span of one actor incarnation.
//!
//! Every call into the user's subscriber that this module makes (creating the
//! span, entering it, leaving it, closing it) runs inside `contain_log`, as
//! every log statement of the crate does, so a subscriber that panics never
//! unwinds through the actor's task: the actor goes on and its reason is
//! unchanged.

#[cfg(feature = "tracing")]
use std::future::Future;
#[cfg(feature = "tracing")]
use std::pin::Pin;
#[cfg(feature = "tracing")]
use std::task::{Context, Poll};

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::types::ActorId;

/// The subscriber's span type, named only beside `caught_log!`.
#[cfg(feature = "tracing")]
type RawSpan = caught_log!(@span_type);

/// The subscriber's span guard type, named only beside `caught_log!`.
#[cfg(feature = "tracing")]
type RawEntered<'a> = caught_log!(@entered_type 'a);

/// The span an actor's task runs in; zero-size without the `tracing`
/// feature.
#[derive(Debug)]
pub(crate) struct ActorSpan {
    /// Asserted unwind safe: the span is only entered and closed by the
    /// crate, each call inside a catch. `None` for a span nobody is
    /// interested in, which no poll enters.
    #[cfg(feature = "tracing")]
    span: Option<std::panic::AssertUnwindSafe<RawSpan>>,
}

/// Drops a panic payload of a subscriber the way `contain_log` does: text
/// payloads are dropped, any other payload is forgotten.
#[cfg(feature = "tracing")]
fn forget_payload(payload: crate::actor::panic::PanicPayload) {
    if payload.is::<&'static str>() || payload.is::<String>() {
        drop(payload);
    } else {
        std::mem::forget(payload);
    }
}

/// A span entered for the length of one poll (or one drop); leaves it,
/// inside a catch, when it drops. Zero-size without the `tracing` feature.
pub(crate) struct PollSpan<'a> {
    #[cfg(feature = "tracing")]
    entered: Option<RawEntered<'a>>,
    #[cfg(not(feature = "tracing"))]
    _span: std::marker::PhantomData<&'a ()>,
}

#[cfg(feature = "tracing")]
impl Drop for PollSpan<'_> {
    fn drop(&mut self) {
        if let Some(entered) = self.entered.take() {
            crate::actor::panic::contain_log(move || drop(entered));
        }
    }
}

#[cfg(feature = "tracing")]
impl Drop for ActorSpan {
    fn drop(&mut self) {
        // Closing the span calls the subscriber's `try_close`.
        if let Some(span) = self.span.take() {
            crate::actor::panic::contain_log(move || drop(span));
        }
    }
}

impl ActorSpan {
    /// The span of a new incarnation: a root span carrying the actor's id,
    /// name, incarnation and system, created only when its callsite is
    /// enabled; `None` when nobody is interested in it, which is the common
    /// case and costs one load.
    ///
    /// The incarnation number and the system's name are read through
    /// closures that run only when the callsite is enabled, so a spawn
    /// nobody traces draws no number and copies no name.
    #[inline]
    pub(crate) fn for_spawn<'s>(
        id: &ActorId,
        name: Option<&str>,
        incarnation: impl FnOnce() -> u64,
        system: impl FnOnce() -> &'s str,
    ) -> Option<Self> {
        #[cfg(feature = "tracing")]
        {
            // With no subscriber that takes debug level, the macro in
            // `traced` would end at the same level check; asking first
            // leaves out the catch and the span it would build.
            if !caught_log!(@level DEBUG) {
                return None;
            }
            Self::traced(id, name, incarnation, system)
        }
        #[cfg(not(feature = "tracing"))]
        {
            let _ = (id, name, incarnation, system);
            None
        }
    }

    /// The span of [`for_spawn`](Self::for_spawn) once some subscriber takes
    /// debug level.
    #[cfg(feature = "tracing")]
    #[inline(never)]
    fn traced<'s>(
        id: &ActorId,
        name: Option<&str>,
        incarnation: impl FnOnce() -> u64,
        system: impl FnOnce() -> &'s str,
    ) -> Option<Self> {
        // The macro evaluates its fields only when the span's callsite is
        // enabled, so a spawn with no interested subscriber builds
        // nothing. A subscriber that panics in `new_span` leaves the
        // actor without a span.
        let span = crate::actor::panic::catch_sync(|| {
            caught_log!(
                @new_span
                id,
                name.unwrap_or(""),
                incarnation(),
                system()
            )
        });
        match span {
            Ok(span) if span.id().is_some() => Some(Self {
                span: Some(std::panic::AssertUnwindSafe(span)),
            }),
            Ok(_) => None,
            Err(payload) => {
                forget_payload(payload);
                None
            }
        }
    }

    /// Whether a subscriber takes any statement that carries the identity
    /// fields of an actor. The query is made at error level, the most severe
    /// level, which every severity filter accepts, so a subscriber that takes only
    /// warnings or errors is interested too. Without the `tracing` feature,
    /// or with no subscriber, a relaxed load or less. A subscriber that panics in the
    /// query reads as not interested.
    #[inline]
    pub(crate) fn log_interest() -> bool {
        #[cfg(feature = "tracing")]
        {
            caught_log!(@level ERROR) && Self::interest_asked()
        }
        #[cfg(not(feature = "tracing"))]
        {
            false
        }
    }

    #[cfg(feature = "tracing")]
    #[inline(never)]
    fn interest_asked() -> bool {
        crate::actor::panic::catch_sync(|| caught_log!(@interest)).unwrap_or(false)
    }

    /// Enters the span until the returned value drops. The subscriber's
    /// `enter` runs inside a catch; a panic of it leaves the span not
    /// entered, and nothing is left for the guard to exit.
    pub(crate) fn enter(&self) -> PollSpan<'_> {
        #[cfg(feature = "tracing")]
        {
            let entered = match &self.span {
                Some(span) => match crate::actor::panic::catch_sync(|| span.0.enter()) {
                    Ok(entered) => Some(entered),
                    Err(payload) => {
                        forget_payload(payload);
                        None
                    }
                },
                None => None,
            };
            PollSpan { entered }
        }
        #[cfg(not(feature = "tracing"))]
        {
            PollSpan {
                _span: std::marker::PhantomData,
            }
        }
    }
}

impl ActorSpan {
    /// Spawns the actor's task `fut` on `rt`. With the `tracing` feature and a
    /// span a subscriber is interested in, every poll of the task, and its
    /// drop, runs inside the span, which belongs to the task: it closes when
    /// the task ends or is dropped, not when the last handle to the actor
    /// goes. Without a span the task is spawned as it is, so a spawn nobody
    /// traces pays no wrapper, no larger future and no branch per poll.
    #[cfg(feature = "tracing")]
    fn spawn_in<F>(self, rt: &Handle, fut: F) -> JoinHandle<F::Output>
    where
        F: Future + Unpin + Send + 'static,
        F::Output: Send + 'static,
    {
        if self.span.is_none() {
            return rt.spawn(fut);
        }
        rt.spawn(SpanRun {
            span: self,
            inner: Some(fut),
        })
    }

    /// See the `tracing` build of this function.
    #[cfg(not(feature = "tracing"))]
    fn spawn_in<F>(self, rt: &Handle, fut: F) -> JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        rt.spawn(fut)
    }
}

/// Spawns an actor's task in the span it was made with, if it has one.
pub(crate) trait SpawnTraced {
    /// Spawns `fut` on `rt`: inside the span when there is one, as it is
    /// when there is none (the common case, which costs a test of the
    /// option).
    fn spawn<F>(self, rt: &Handle, fut: F) -> JoinHandle<F::Output>
    where
        F: std::future::Future + Unpin + Send + 'static,
        F::Output: Send + 'static;
}

impl SpawnTraced for Option<ActorSpan> {
    #[inline]
    fn spawn<F>(self, rt: &Handle, fut: F) -> JoinHandle<F::Output>
    where
        F: std::future::Future + Unpin + Send + 'static,
        F::Output: Send + 'static,
    {
        match self {
            Some(span) => span.spawn_in(rt, fut),
            None => rt.spawn(fut),
        }
    }
}

/// A future that runs every poll of the inner future, and its drop, inside
/// the incarnation's span, which it owns. Replaces `tracing::Instrument`,
/// whose enter and exit call the subscriber outside any catch.
#[cfg(feature = "tracing")]
struct SpanRun<F> {
    span: ActorSpan,
    /// Taken at the drop, so the inner future drops inside the span.
    inner: Option<F>,
}

#[cfg(feature = "tracing")]
impl<F: Future + Unpin> Future for SpanRun<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        let _entered = this.span.enter();
        let inner = this
            .inner
            .as_mut()
            .expect("the inner future is held until the drop");
        Pin::new(inner).poll(cx)
    }
}

#[cfg(feature = "tracing")]
impl<F> Drop for SpanRun<F> {
    fn drop(&mut self) {
        let _entered = self.span.enter();
        drop(self.inner.take());
    }
}
