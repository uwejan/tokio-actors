//! The abort an incarnation is owed and the path that issues it.

use super::block::*;
use super::subtree::*;
use std::sync::Arc;
use tokio::task::AbortHandle;
use tokio::time::Instant;

#[derive(Debug, Default)]
pub(super) struct Owed {
    /// The instant of the first abort issued, written once.
    pub(super) at: Option<Instant>,
    /// Boxed, so a record that is owed nothing (nearly every one) holds one
    /// pointer for it.
    pub(super) mark: Option<Box<OwedMark>>,
    /// The actor ended: nothing is owed to it any more.
    pub(super) ended: bool,
}

/// One owed abort, counted on the shared state of the incarnation's token
/// and every ancestor's; its drop takes the count back.
#[derive(Debug)]
pub(super) struct OwedMark {
    pub(super) chain: Vec<Arc<SubtreeShared>>,
}

impl Drop for OwedMark {
    fn drop(&mut self) {
        for shared in &self.chain {
            shared.update_aborts(|a| a.owed = a.owed.saturating_sub(1));
        }
    }
}

impl IncarnationRef {
    /// The shared state of the linked token and of every ancestor's.
    pub(super) fn chain(&self) -> Vec<Arc<SubtreeShared>> {
        let mut chain = Vec::new();
        let token = self
            .shared
            .linked
            .get()
            .and_then(|linked| linked.token.upgrade());
        #[cfg(test)]
        test_hook::run_in_chain();
        let mut at = token.as_deref();
        while let Some(current) = at {
            chain.push(current.shared.clone());
            at = current.parent.as_deref();
        }
        chain
    }

    /// An abort is now owed to the task: marks it unless an abort was
    /// already issued, a mark is already held, or the actor ended.
    pub(crate) fn owe(&self) {
        let mut owed = self.shared.owed.lock().unwrap_or_else(|p| p.into_inner());
        if owed.ended || owed.mark.is_some() || owed.at.is_some() {
            return;
        }
        let chain = self.chain();
        for shared in &chain {
            shared.update_aborts(|a| a.owed = a.owed.saturating_add(1));
        }
        owed.mark = Some(Box::new(OwedMark { chain }));
    }

    /// An abort is now owed to `task`: marks it as [`IncarnationRef::owe`]
    /// does, unless the task already finished, in which case nothing is owed
    /// and the mark is released as [`IncarnationRef::abort`] releases it. A
    /// hold that outlives the task (a forwarder still ending) keeps no mark
    /// alive over a task that ended before the owe.
    pub(crate) fn owe_for(&self, task: &AbortHandle) {
        if task.is_finished() {
            self.release();
        } else {
            self.owe();
        }
    }

    /// A party that watches the incarnation's task saw it end (a ladder's
    /// join, an observer of the status plane's end): releases the owed mark
    /// and takes none again, exactly as the actor's own end releases it, so
    /// a hold that outlives the task keeps no mark alive. Idempotent.
    pub(crate) fn task_ended(&self) {
        self.release();
    }

    /// Aborts `task`, recording the instant unless an abort was recorded
    /// already. Every abort the crate issues against an incarnation's task
    /// goes through here. A task that already finished gets no abort and no
    /// record: nothing is owed to a task that ended, so the owed mark is
    /// released instead, as the actor's end releases it, and a hold that
    /// outlives the task (a forwarder still ending) keeps no mark alive.
    ///
    /// The abort that writes the first instant of an unfinished task reports
    /// the `AbortFired` step, and no other does: the instant is written once
    /// per incarnation under the record's lock, so two aborts that race (the
    /// reaper's and a caller's own) report one step. The event is built
    /// before the lock is taken and queued inside it, right after the instant
    /// is written, so an abort that lost the race passes the lock only once
    /// the step is queued and cannot end the task ahead of it: the step
    /// precedes the events of the actor's end whichever abort ends the task.
    pub(crate) fn abort(&self, task: &AbortHandle) {
        if task.is_finished() {
            self.release();
            return;
        }
        let staged = crate::system::stage_abort_fired(self);
        self.record(Instant::now(), staged);
        task.abort();
    }

    /// Records `now` as the first abort and returns true, or returns false
    /// when an abort was recorded already, which stands. The first abort
    /// queues `staged` while it holds the lock: the push is a bounded queue
    /// push that never waits and runs no user code.
    ///
    /// The chain of a record with no owed mark is read inside the critical
    /// section, and not at all once the actor has ended: the release of the
    /// actor's hold takes this lock before it lets go of the token, so no
    /// reader of the chain holds the token when the hold's release is
    /// written (see [`ReleaseSignal`]). An abort recorded after the actor
    /// ended reaches no ancestor: nothing is left of it to wait for.
    pub(super) fn record(&self, now: Instant, staged: Option<crate::telemetry::Staged>) -> bool {
        let (mark, chain) = {
            let mut owed = self.shared.owed.lock().unwrap_or_else(|p| p.into_inner());
            if owed.at.is_some() {
                return false;
            }
            owed.at = Some(now);
            #[cfg(test)]
            test_hook::run_before_push();
            if let Some(staged) = staged {
                staged.push();
            }
            let mark = owed.mark.take();
            let chain = match &mark {
                Some(mark) => mark.chain.clone(),
                None if owed.ended => Vec::new(),
                None => self.chain(),
            };
            (mark, chain)
        };
        #[cfg(test)]
        test_hook::run_after_write();
        self.notify_signals();
        for shared in &chain {
            shared.update_aborts(|a| a.latest = Some(a.latest.map_or(now, |l| l.max(now))));
        }
        drop(mark);
        true
    }

    /// The actor ended: releases the owed mark, and none is taken again.
    ///
    /// The end of an actor that was owed an abort counts as that abort for
    /// the waits on its subtree. Its end cancels the runners of its children,
    /// whose guards owe their aborts only once the runtime drops those
    /// runners, so the subtree needs the same settle time after the end that
    /// it gets after a recorded abort. The mark is taken and the end written
    /// in one critical section, the instant of the end is raised as the
    /// latest abort on every token the mark counted on, and only then does
    /// the mark drop: no reader sees the subtree with nothing owed and
    /// nothing recorded in between. With no mark held, nothing is raised.
    pub(super) fn release(&self) {
        let mark = {
            let mut owed = self.shared.owed.lock().unwrap_or_else(|p| p.into_inner());
            owed.ended = true;
            owed.mark.take()
        };
        if let Some(mark) = &mark {
            let now = Instant::now();
            for shared in &mark.chain {
                shared.update_aborts(|a| a.latest = Some(a.latest.map_or(now, |l| l.max(now))));
            }
        }
        drop(mark);
    }

    /// The first abort's instant, and whether an abort is owed and not yet
    /// issued. The instant is written and the mark taken in one critical
    /// section, so an abort is never reported both recorded and owed.
    pub(crate) fn view(&self) -> (Option<Instant>, bool) {
        let read = {
            let owed = self.shared.owed.lock().unwrap_or_else(|p| p.into_inner());
            (owed.at, owed.mark.is_some())
        };
        #[cfg(test)]
        test_hook::run();
        read
    }

    /// A future woken when the first abort is recorded (and by every other
    /// change the block's signals announce); create it before reading the
    /// record so no record is missed, and read the record again after each
    /// wake.
    pub(crate) fn recorded(&self) -> tokio::sync::futures::Notified<'_> {
        self.shared.signals.notified()
    }
}
