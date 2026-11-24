//! The count of live subtree holds of a system that an application owns.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::actor::panic::contain_waker_panic;

/// The live-hold count of a system an application controller created for an
/// application of its own: a flag fixed at the system's creation, the number
/// of subtree holds of its actors that are alive, and the wake of a wait for
/// that number to reach zero, alone on a cache line. Every other system reads
/// the flag only, at a hold's build and at its drop.
#[derive(Debug, Default)]
#[repr(align(64))]
pub(crate) struct LiveHolds {
    /// Set once, before the system can be reached, for a system of an
    /// application's own; never cleared.
    counted: std::sync::atomic::AtomicBool,
    /// The holds built and not yet dropped, counted only under `counted`.
    live: AtomicUsize,
    /// Woken when a drop brings `live` to zero.
    zero: tokio::sync::Notify,
}

impl LiveHolds {
    /// Counts the holds of this system from here on. Called by the system's
    /// constructor, before any actor can be spawned in it.
    pub(crate) fn mark_counted(&self) {
        self.counted.store(true, Ordering::Release);
    }

    /// A hold of an actor of this system was built.
    #[inline]
    pub(crate) fn built(&self) {
        if self.counted.load(Ordering::Relaxed) {
            self.live.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// A hold of an actor of this system was dropped, after its release. The
    /// drop that brings the count to zero wakes every wait on it; the wakers
    /// can belong to any task, so the wake runs inside the crate's catch.
    #[inline]
    pub(crate) fn dropped(&self) {
        if self.counted.load(Ordering::Relaxed) && self.live.fetch_sub(1, Ordering::AcqRel) == 1 {
            contain_waker_panic("live holds", None, || self.zero.notify_waiters());
        }
    }

    /// The holds alive now.
    pub(crate) fn live_holds(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// Resolves when the count reads zero, at once when it does now. It reads
    /// no clock; the caller bounds it.
    pub(crate) async fn holds_released(&self) {
        loop {
            let notified = self.zero.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.live_holds() == 0 {
                return;
            }
            notified.await;
        }
    }
}
