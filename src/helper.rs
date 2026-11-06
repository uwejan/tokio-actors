//! The helper tasks of the crate and the slot every one of them is spawned
//! through.
//!
//! A helper task is a task the crate runs for no actor in particular: the
//! reaper that aborts what outlives its grace, the turn-watchdog sweep, the
//! task that drops what arrives at an actor's queue after the actor ended, and
//! the three tasks that carry the steps of a call: the start bound of a spawn
//! with a start timeout, the stop ladder of a bounded stop, and the shutdown
//! walk. A system keeps one slot for each runtime its helpers run on. The
//! slot sees every helper start and every helper end, whichever way it ends:
//!
//! - a task that runs to its end is counted down;
//! - a task whose poll panicked is counted down and its panic is logged, with
//!   the owner's id when the task has an owner;
//! - a task that is dropped before its end (its runtime stopped, or it was
//!   aborted) is counted down, and the values it dropped unsettled are logged
//!   with the owner's id.
//!
//! Each end is recorded once, from the drop glue of the task's own guard, so
//! a task that never ran its first poll is recorded too. The reaper, the
//! sweep and the late-value drain never hold the only copy of an obligation:
//! what they settle is also settled by the end of whatever feeds them, and no
//! user code runs inside the reaper or the sweep.
//!
//! The three tasks that carry the steps of a call do hold the only copy of
//! those steps while they run, and each fails closed. The start bound aborts
//! the start when it panics before the start was acknowledged. The stop
//! ladder hands the steps it still owes to the reaper of the actor's own
//! runtime when it panics or its runtime ends. The shutdown walk is walked
//! again by its caller when it panics. The one case in which a step is cut
//! short is the end of the runtime a task runs on: the actor of a start bound
//! ends with that runtime, and a shutdown walk whose caller has stopped waiting
//! leaves the system refusing registrations until a shutdown of the system has
//! completed (an earlier one counts).

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::runtime::{Handle, Id};
use tokio::task::JoinHandle;

use crate::actor::panic::{catch_callback, payload_into_string};
use crate::types::ActorId;

/// Which helper a task is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HelperKind {
    /// Drops what arrives after a queue closed: a push that was landing, or,
    /// on a lane, one through a slot reserved before the close.
    LateDrain,
    /// Aborts the tasks that outlive their grace.
    Reaper,
    /// Reports the turns that run longer than the system's threshold.
    #[cfg_attr(not(feature = "tracing"), allow(dead_code))]
    Sweep,
    /// Cuts the start of a spawn that outlives its start timeout.
    StartBound,
    /// Runs the escalation ladder of one bounded stop after its caller has
    /// raised the first request.
    StopLadder,
    /// Walks a system shutdown to its end.
    ShutdownWalk,
}

impl HelperKind {
    #[cfg(feature = "tracing")]
    fn name(self) -> &'static str {
        match self {
            Self::LateDrain => "late-value drain",
            Self::Reaper => "reaper",
            Self::Sweep => "turn-watchdog sweep",
            Self::StartBound => "start bound",
            Self::StopLadder => "stop ladder",
            Self::ShutdownWalk => "shutdown walk",
        }
    }
}

/// How a helper task ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum HelperEnd {
    /// The task ran to its end.
    Finished,
    /// A poll of the task panicked.
    Panicked,
    /// The task was dropped before its end; `dropped` is the number of late
    /// values it held and dropped unsettled.
    Cancelled { dropped: usize },
}

/// The slots of one system, one for each runtime its helpers run on.
#[derive(Debug, Default)]
pub(crate) struct HelperSlots {
    by_runtime: Mutex<HashMap<Id, Arc<HelperSlot>>>,
}

/// The helper tasks of one system on one runtime.
#[derive(Debug, Default)]
pub(crate) struct HelperSlot {
    live: AtomicUsize,
    #[cfg(test)]
    ends: Mutex<Vec<(HelperKind, HelperEnd)>>,
}

impl HelperSlots {
    /// The guard of a helper about to start on `rt`; the helper counts as
    /// live from here until the guard records its end.
    fn guard(&self, rt: &Handle, kind: HelperKind, owner: Option<ActorId>) -> EndGuard {
        let id = rt.id();
        let slot = {
            let mut slots = self
                .by_runtime
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match slots.get(&id) {
                Some(slot) => slot.clone(),
                None => {
                    // A slot with no guard out serves no task: its runtime
                    // ended, or every helper on it finished.
                    slots.retain(|_, slot| Arc::strong_count(slot) > 1);
                    let slot = Arc::new(HelperSlot::default());
                    slots.insert(id, slot.clone());
                    slot
                }
            }
        };
        slot.live.fetch_add(1, Ordering::AcqRel);
        EndGuard {
            slot,
            kind,
            owner,
            recorded: false,
        }
    }

    /// The helpers of this system that have started and not ended.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.by_runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|slot| slot.live.load(Ordering::Acquire))
            .sum()
    }

    /// The ends recorded on `rt` so far, in order.
    #[cfg(test)]
    pub(crate) fn ends(&self, rt: &Handle) -> Vec<(HelperKind, HelperEnd)> {
        let slot = self
            .by_runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&rt.id())
            .cloned();
        slot.map(|slot| {
            slot.ends
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        })
        .unwrap_or_default()
    }

    /// Spawns a helper on `rt` through the slot. `make` builds the task's
    /// future from the guard that records its end: the future holds the guard
    /// for its whole life, so every way it can end is recorded.
    pub(crate) fn spawn<F, Fut>(
        &self,
        rt: &Handle,
        kind: HelperKind,
        owner: Option<ActorId>,
        make: F,
    ) -> JoinHandle<()>
    where
        F: FnOnce(EndGuard) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        rt.spawn(make(self.guard(rt, kind, owner)))
    }
}

/// Logs one line about a helper's end, naming its owner when it has one.
#[cfg(feature = "tracing")]
macro_rules! helper_log {
    ($guard:expr, $($arg:tt)+) => {
        match &$guard.owner {
            Some(owner) => caught_log!(warn,
                actor_id = %owner,
                helper = $guard.kind.name(),
                $($arg)+
            ),
            None => caught_log!(warn,
                helper = $guard.kind.name(),
                $($arg)+
            ),
        }
    };
}

/// Records the end of one helper task, exactly once.
#[derive(Debug)]
pub(crate) struct EndGuard {
    slot: Arc<HelperSlot>,
    /// Named by the log lines of an end, which the `tracing` feature writes.
    #[cfg_attr(not(feature = "tracing"), allow(dead_code))]
    kind: HelperKind,
    #[cfg_attr(not(feature = "tracing"), allow(dead_code))]
    owner: Option<ActorId>,
    recorded: bool,
}

impl EndGuard {
    /// Runs `fut` as the whole of the helper: a panic of a poll is contained
    /// and recorded, and a completion is recorded. If the returned future is
    /// dropped before it ends, the guard records the cancellation.
    pub(crate) async fn observe<F: Future<Output = ()>>(self, fut: F) {
        match catch_callback(fut).await {
            Ok(()) => self.finished(),
            Err(payload) => self.panicked(&payload_into_string(payload)),
        }
    }

    /// The task ran to its end.
    pub(crate) fn finished(mut self) {
        self.record(HelperEnd::Finished);
    }

    /// A poll of the task panicked; the panic is logged.
    pub(crate) fn panicked(mut self, payload: &str) {
        #[cfg(feature = "tracing")]
        helper_log!(
            self,
            panic = %payload,
            "a helper task of the crate panicked; the panic is contained"
        );
        #[cfg(not(feature = "tracing"))]
        let _ = payload;
        self.record(HelperEnd::Panicked);
    }

    /// The task is dropped before its end, and `dropped` late values it held
    /// are dropped with it unsettled.
    pub(crate) fn cancelled(mut self, dropped: usize) {
        self.record_cancelled(dropped);
    }

    fn record_cancelled(&mut self, dropped: usize) {
        #[cfg(feature = "tracing")]
        if dropped > 0 {
            helper_log!(
                self,
                dropped,
                "a helper task of the crate ended before it settled; the late values it held were dropped"
            );
        }
        self.record(HelperEnd::Cancelled { dropped });
    }

    fn record(&mut self, end: HelperEnd) {
        if std::mem::replace(&mut self.recorded, true) {
            return;
        }
        #[cfg(test)]
        self.slot
            .ends
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((self.kind, end));
        #[cfg(not(test))]
        let _ = end;
        self.slot.live.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        if !self.recorded {
            self.record_cancelled(0);
        }
    }
}

#[cfg(all(test, feature = "tracing"))]
mod end_log_tests;
