//! The bounded queue and helper task that deliver events raised where no
//! user code may run.
//!
//! The reaper, the abort steps of a `Kill`, the end of a helper task and every
//! other event of the library go through here. A push builds nothing when no
//! sink takes the event, takes the queue's lock (and, for an event it drops,
//! the registry lock for a moment), never waits for room and never runs user
//! code, so it is safe in a drop, under a registry guard and on the reaper
//! task. One helper per system, started through the helper slot on the first
//! push, drains the queue into the sinks. An event that is given up on (the
//! queue is full, the system is gone, it was raised inside a handler of the
//! system, or it was still queued when the system's telemetry state dropped)
//! goes through [`Pump::drop_event`]: counted once for the handlers' report and
//! carried to the subscriptions that would have taken it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::runtime::Handle;
use tokio::sync::Notify;

use super::event::{EventKind, TelemetryEvent};
use super::handlers::{drop_contained, HandlerEntry};
use super::identity::Identity;
use super::mode::Mode;
use super::subscription;
use crate::actor::panic::{catch_sync, discard};
use crate::helper::{EndGuard, HelperKind};
use crate::system::ActorSystem;

/// The most events the pump holds; later ones are counted and dropped.
pub(crate) const PUMP_CAPACITY: usize = 4096;

/// One queued event, with the identity of the actor it is about when the
/// pusher had it: that identity's count of events in the pump was raised when
/// the event was accepted and is lowered when its delivery ends or it is given
/// up on.
type Queued = (EventKind, TelemetryEvent<'static>, Option<Arc<Identity>>);

/// The pump's queue and state.
#[derive(Debug, Default)]
pub(crate) struct Pump {
    queue: Mutex<VecDeque<Queued>>,
    /// Handler lists given up by a loop's cache, to be dropped on the helper:
    /// the state the handlers captured is user code.
    released: Mutex<Vec<Arc<[HandlerEntry]>>>,
    notify: Notify,
    /// Events dropped so far, in every build.
    dropped: AtomicU64,
    /// How much of `dropped` the helper has reported; written by the helper
    /// only.
    reported: AtomicU64,
    running: AtomicBool,
    closed: AtomicBool,
    #[cfg(test)]
    pushed: AtomicU64,
    /// While set, the helper takes nothing, so a test holds a backlog.
    #[cfg(test)]
    held: AtomicBool,
}

impl Pump {
    /// Queues `event` for the helper, dropping and counting it when the queue
    /// is full, and starts the helper if it is not running. Never waits.
    pub(crate) fn enqueue(
        &self,
        mode: &Arc<Mode>,
        kind: EventKind,
        event: TelemetryEvent<'static>,
    ) {
        self.enqueue_for(mode, kind, event, None);
    }

    /// [`enqueue`](Self::enqueue) for an event about the actor `owner`: the
    /// actor's count of events in the pump is raised under the queue's lock,
    /// before the event can be taken, and lowered after its delivery, so an
    /// actor that reads the count as zero knows every event it pushed has been
    /// delivered.
    pub(crate) fn enqueue_for(
        &self,
        mode: &Arc<Mode>,
        kind: EventKind,
        event: TelemetryEvent<'static>,
        owner: Option<Arc<Identity>>,
    ) {
        #[cfg(test)]
        self.pushed.fetch_add(1, Ordering::Relaxed);
        // An event raised after the system is gone has no helper to start
        // and no sink to reach: it is dropped and counted.
        if mode.system_gone() {
            self.drop_event(mode, kind, &event);
            return;
        }
        let rejected = {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if queue.len() >= PUMP_CAPACITY {
                Some(event)
            } else {
                if let Some(owner) = &owner {
                    owner.raise_pumped();
                }
                queue.push_back((kind, event, owner));
                None
            }
        };
        if let Some(event) = rejected {
            self.drop_event(mode, kind, &event);
        }
        self.notify.notify_one();
        Self::ensure_started(mode);
    }

    /// Takes a handler list a loop's cache gave up, to drop it on the helper
    /// and not on the path that ended the loop. Never waits and runs no user
    /// code. A pump that is closed, or whose system is gone, drops the list
    /// where it is, inside a catch.
    pub(crate) fn release(&self, mode: &Arc<Mode>, entries: Arc<[HandlerEntry]>) {
        if mode.system_gone() || self.closed.load(Ordering::Acquire) {
            drop_contained(entries);
            return;
        }
        self.released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entries);
        self.notify.notify_one();
        Self::ensure_started(mode);
    }

    /// Gives up on `event`: counts it once for the report to the handlers and
    /// carries it into the drop count of every subscription whose filter would
    /// have taken it. The one place a built event is dropped; never waits for
    /// room and runs no user code.
    pub(crate) fn drop_event(&self, mode: &Mode, kind: EventKind, event: &TelemetryEvent<'static>) {
        self.count_dropped();
        subscription::carry_drop(mode, kind, event);
    }

    /// Gives up on every event still queued, when the system's telemetry state
    /// is dropped and no helper can run any more: each is counted and carried
    /// through [`drop_event`](Self::drop_event). Takes the queue's lock only to
    /// empty it.
    pub(crate) fn discard_queued(&self, mode: &Mode) {
        let left: Vec<_> = {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            queue.drain(..).collect()
        };
        for (kind, event, owner) in left {
            self.drop_event(mode, kind, &event);
            if let Some(owner) = owner {
                owner.lower_pumped();
            }
        }
        for entries in self.take_released() {
            drop_contained(entries);
        }
    }

    /// Takes the handler lists waiting to be dropped.
    fn take_released(&self) -> Vec<Arc<[HandlerEntry]>> {
        std::mem::take(
            &mut *self
                .released
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// Counts one event as dropped, for the report to the handlers. Alone it
    /// serves an event the subscriptions already received and only the
    /// handlers missed.
    pub(crate) fn count_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Starts the pump helper once on the current runtime, through the
    /// system's helper slot. Does nothing when the helper runs, the pump is
    /// closed, no runtime is current or the system is gone.
    pub(crate) fn ensure_started(mode: &Arc<Mode>) {
        let pump = &mode.pump;
        if pump.running.load(Ordering::Acquire) || pump.closed.load(Ordering::Acquire) {
            return;
        }
        let Ok(rt) = Handle::try_current() else {
            return;
        };
        let Some(system) = mode.system() else {
            return;
        };
        if pump
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let weak = Arc::downgrade(&system);
        let mode = mode.clone();
        drop(
            system.spawn_helper(&rt, HelperKind::Pump, None, move |guard| {
                // Built with the future, so a task dropped before its first
                // poll clears the flag too.
                let reset = RunningReset(mode.clone());
                pump_run(mode, weak, guard, reset)
            }),
        );
    }

    /// Ends the helper once its queue is empty.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    /// Takes everything queued.
    fn take(&self, into: &mut Vec<Queued>) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        into.extend(queue.drain(..));
    }

    /// The number of events dropped and not yet reported.
    fn unreported(&self) -> u64 {
        self.dropped
            .load(Ordering::Relaxed)
            .saturating_sub(self.reported.load(Ordering::Relaxed))
    }

    /// The count of events dropped, for a full queue or a system that was
    /// gone.
    #[cfg(test)]
    pub(crate) fn dropped_for_test(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The number of events queued and not yet taken by the helper.
    #[cfg(test)]
    pub(crate) fn queued_for_test(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// The number of events that reached `enqueue`.
    #[cfg(test)]
    pub(crate) fn pushed_for_test(&self) -> u64 {
        self.pushed.load(Ordering::Relaxed)
    }

    /// Whether the helper is started and has not ended.
    #[cfg(test)]
    pub(crate) fn running_for_test(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Makes the helper take nothing until released, so a test holds a
    /// backlog while the helper runs.
    #[cfg(test)]
    pub(crate) fn hold_for_test(&self, on: bool) {
        self.held.store(on, Ordering::Release);
        self.notify.notify_one();
    }

    /// The owners of the queued events of `kind`, oldest first: `None` for an
    /// event no actor's count includes.
    #[cfg(test)]
    pub(crate) fn queued_owners_for_test(
        &self,
        kind: EventKind,
    ) -> Vec<Option<crate::types::ActorId>> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(queued, _, _)| *queued == kind)
            .map(|(_, _, owner)| owner.as_ref().map(|owner| owner.actor().id().clone()))
            .collect()
    }

    /// Marks the helper as running without starting one, so a test fills the
    /// queue with no consumer.
    #[cfg(test)]
    pub(crate) fn stall_for_test(&self) {
        self.running.store(true, Ordering::Release);
    }

    /// Undoes [`stall_for_test`](Self::stall_for_test), so the next start
    /// spawns the helper.
    #[cfg(test)]
    pub(crate) fn unstall_for_test(&self) {
        self.running.store(false, Ordering::Release);
    }
}

/// Clears the helper's running flag when its task ends in any way, so a later
/// push on a live runtime starts a new one.
pub(crate) struct RunningReset(Arc<Mode>);

impl Drop for RunningReset {
    fn drop(&mut self) {
        self.0.pump.running.store(false, Ordering::Release);
    }
}

/// The pump helper: drains the queue into the sinks until the pump is closed
/// or the system is gone and the queue is empty. `_running` is a parameter so
/// the future owns it from its construction.
pub(crate) async fn pump_run(
    mode: Arc<Mode>,
    weak: Weak<ActorSystem>,
    guard: EndGuard,
    _running: RunningReset,
) {
    guard.observe(drain(&mode, &weak)).await;
}

/// Reports the events dropped since the last report: one `Dropped` event to
/// the sinks that take it, and with the `tracing` feature one warning.
fn report_dropped(mode: &Arc<Mode>) {
    let pump = &mode.pump;
    let missed = pump.unreported();
    if missed == 0 {
        return;
    }
    pump.reported.fetch_add(missed, Ordering::Relaxed);
    #[cfg(feature = "tracing")]
    caught_log!(
        warn,
        missed,
        "telemetry events were dropped: the pump's queue was full, its system was gone, or they were raised inside a handler"
    );
    mode.dispatch(EventKind::Dropped, || TelemetryEvent::Dropped { missed });
}

async fn drain(mode: &Arc<Mode>, weak: &Weak<ActorSystem>) {
    let pump = &mode.pump;
    let mut batch = Vec::new();
    loop {
        #[cfg(test)]
        while pump.held.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        // Read before the queue is taken: an event pushed before the close is
        // delivered, and the helper ends only on an empty queue.
        let ending = pump.closed.load(Ordering::Acquire) || weak.strong_count() == 0;
        pump.take(&mut batch);
        let released = pump.take_released();
        let idle = batch.is_empty() && released.is_empty();
        for entries in released {
            drop_contained(entries);
        }
        for (kind, event, owner) in batch.drain(..) {
            // The sinks contain a panic of user code themselves; this catch
            // keeps one bad delivery from ending the helper.
            if let Err(payload) = catch_sync(|| mode.sinks.deliver(mode, kind, event)) {
                discard(payload);
            }
            if let Some(owner) = owner {
                owner.lower_pumped();
            }
        }
        if let Err(payload) = catch_sync(|| report_dropped(mode)) {
            discard(payload);
        }
        if idle {
            if ending {
                return;
            }
            pump.notify.notified().await;
            continue;
        }
        tokio::task::yield_now().await;
    }
}
