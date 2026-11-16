//! Subtree tokens: how a restart, a `terminate_child`, a `spawn_child` under
//! the id of a removed child and a subtree observer learn that every actor an
//! incarnation spawned has ended.
//!
//! Every spawned actor holds a token. A token is held by the actor's run
//! loop, its context, its registry guard and its timer and stream
//! forwarders, and by the token of each actor it spawned as their parent, so
//! a token is dropped only when the actor, everything it owned and everything
//! below it have ended. Its drop glue signals `gone`, and drop glue runs when
//! a task is aborted too, so the signal fires on every way an actor can end.
//! The hold also signals `released` as the last step of its own drop, after it
//! let go of its reference to the token: the actor let go of everything it
//! held, and the actors below it may still run. A wait that needs no timer for
//! the actor's own end waits for `released`, and one that finds `released`
//! without `gone` knows that something else still holds the token.
//!
//! A token also carries the budget of its subtree: the time that subtree can
//! outlive a killed root. Creating a child raises its parent's budget to at
//! least the child's own budget plus the child's subtree, and a raise
//! propagates to the root. The budget never decreases, so the bound stays
//! conservative when a child ends.
//!
//! The count of live supervisor children whose orderly stop has no bound is
//! kept beside the budget. A wait is unbounded only while that count is above
//! zero or the budget is too large to represent, and bounded again, counted
//! from the moment the count reaches zero, by what the subtree still carries.
//!
//! A token also carries what the aborts of its subtree's actors say about a
//! verdict on it: how many live actors below it (itself included) are owed an
//! abort that was not issued yet, and the latest instant an abort was issued
//! against one of them. A wait on the subtree gives no Unresponsive verdict
//! while an abort is owed, up to its own bound once more past its deadline,
//! nor before the latest abort plus the settle time: an abort that is late
//! holds the wait for at most the bound once more plus one more settle time,
//! and one recorded past that bound holds it no further. The end of an actor
//! that was owed an abort counts as that abort. The actor's own end releases
//! its mark, and so does each of these when it sees the task end: the join
//! arm of the supervisor's stop ladder and of its restart attempt, the two
//! observers of a lost runner, the reaper's entry, and the drop of the hold,
//! because a hold a forwarder keeps can outlive the task.
//!
//! Two lists of waiters sit on one token, and a change wakes them one after
//! the other, each under a catch of its own. The first is the library's own: a
//! restart attempt's wait for the previous incarnation's subtree, and the
//! waits of `terminate_child` and `spawn_child`; every task that parks there is
//! one the library polls, with tokio's own waker. The second is the subtree
//! observers' (see `SubtreeObserver`), whose tasks are users' and whose wakers
//! can panic. Tokio owns each list's batch and ends its wake at a waker that
//! panics, leaving the waiters behind it in that list unwoken; the library's
//! list holds no user's waker, so a panicking observer's waker can strand no
//! wait of the library, and strands only the observers behind it.

use super::block::*;
#[cfg(test)]
#[cfg_attr(
    not(all(target_arch = "x86_64", target_os = "linux")),
    allow(unused_imports)
)]
use super::owed::*;
#[cfg(test)]
use super::signals::*;
use crate::actor::panic::contain_waker_panic;
use crate::actor::runtime::{bounded_wait, saturating_deadline};
use crate::actor::supervision::rules;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::time::{sleep_until, Instant};

/// The state a token and every watch of it share.
#[derive(Debug)]
pub(crate) struct SubtreeShared {
    /// The budget of the subtree below this token, in nanoseconds. A value of
    /// `u64::MAX` is unbounded.
    pub(super) below_nanos: AtomicU64,
    /// Whether the actor's own hold has been released: written once, by the
    /// last step of the hold's drop (see [`ReleaseSignal`]), after the hold's
    /// reference to the token dropped. A reader that finds it set and `gone`
    /// not set knows that something else still holds the token: an actor
    /// below the observed one, or a restart of one that is not dropped yet.
    pub(super) released: AtomicBool,
    /// The token's signal (`gone`, the live unbounded-supervisor count and
    /// the generations a wait compares) and the aborts owed to and issued
    /// against the live actors of the subtree, under one lock.
    pub(super) state: Mutex<SubtreeState>,
    /// The library's own waits (a restart attempt's, `terminate_child`'s and
    /// `spawn_child`'s), woken after every change of `state`. A wait creates
    /// its `Notified` before it reads `state`, and `notify_waiters` reaches
    /// every `Notified` created before the call, so a change after the read
    /// wakes it. No user's waker parks here.
    pub(super) changed: tokio::sync::Notify,
    /// The subtree observers' waits, woken after every change of `state`,
    /// after `changed`, and read the same way. A user's waker parks here, and
    /// a panic of one ends tokio's wake of this list only.
    pub(super) observers: tokio::sync::Notify,
    /// Set, never cleared, when the first observer's wait is created (before
    /// it registers and reads `state`). A change wakes the observers' list
    /// only once it is set, so a subtree nobody observes pays nothing for the
    /// list. An observer sets it before it reads `state`, and a change reads
    /// it after it wrote `state`: either the observer's read sees the change,
    /// or the change sees the flag and wakes the list the observer is on.
    pub(super) observed: AtomicBool,
}

#[derive(Debug, Default)]
pub(super) struct SubtreeState {
    pub(super) signal: SubtreeSignal,
    pub(super) aborts: SubtreeAborts,
}

/// The fields of a token's one signal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubtreeSignal {
    pub(super) gone: bool,
    pub(super) inf_live: u32,
    /// Bumped on every change of `inf_live`, so a net-zero change between
    /// two reads still re-arms a wait.
    pub(super) inf_gen: u64,
    pub(super) below_gen: u64,
}

/// What a subtree's aborts say about a verdict on it.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct SubtreeAborts {
    /// Live actors of the subtree, its root included, that are owed an abort
    /// none has issued yet.
    pub(super) owed: u32,
    /// The latest first abort issued against an actor of the subtree.
    pub(super) latest: Option<Instant>,
}

/// The site the log names when a subtree observer's waker panics.
pub(super) const OBSERVERS: &str = "subtree observers";

impl SubtreeShared {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            below_nanos: AtomicU64::new(0),
            released: AtomicBool::new(false),
            state: Mutex::new(SubtreeState::default()),
            changed: tokio::sync::Notify::new(),
            observers: tokio::sync::Notify::new(),
            observed: AtomicBool::new(false),
        })
    }

    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, SubtreeState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wakes every wait whose `Notified` was created before this call: the
    /// library's own waits, then the observers', each list under a catch of
    /// its own. The observers' wakers belong to the observers' tasks, and a
    /// panic of one is kept out of the change that woke it. Tokio ends the wake
    /// of a list at the waker that panics, so the observers behind it stay
    /// asleep; the library's waits are on the other list and are never among
    /// them.
    pub(super) fn wake_waits(&self, site: &'static str) {
        contain_waker_panic(site, None, || self.changed.notify_waiters());
        if self.observed.load(Ordering::SeqCst) {
            contain_waker_panic(OBSERVERS, None, || self.observers.notify_waiters());
        }
    }

    /// The `Notified` of a wait on this signal: the observers' list for a
    /// subtree observer's wait, the library's for any other.
    pub(super) fn notified(&self, observer: bool) -> tokio::sync::futures::Notified<'_> {
        if observer {
            self.observed.store(true, Ordering::SeqCst);
            self.observers.notified()
        } else {
            self.changed.notified()
        }
    }

    /// Applies `change` to the signal, then wakes every wait whose `Notified`
    /// was created before this call.
    pub(super) fn modify(&self, change: impl FnOnce(&mut SubtreeSignal)) {
        change(&mut self.lock().signal);
        self.wake_waits("subtree change");
    }

    pub(super) fn signal(&self) -> SubtreeSignal {
        let read = self.lock().signal;
        #[cfg(test)]
        test_hook::run();
        read
    }

    pub(super) fn update_aborts(&self, change: impl FnOnce(&mut SubtreeAborts)) {
        {
            let mut state = self.lock();
            change(&mut state.aborts);
            state.signal.below_gen = state.signal.below_gen.wrapping_add(1);
        }
        self.wake_waits("subtree change");
    }

    pub(super) fn aborts(&self) -> SubtreeAborts {
        self.lock().aborts
    }
}

/// What a record holds of its incarnation's subtree once the actor's hold
/// exists: the token weakly, so the record never keeps a subtree alive, and
/// the token's shared state strongly. The shared state is counters and a
/// notification and pins no actor, name, id or token, so a reader that comes
/// after the subtree ended still finds the signal that says so, and reads it
/// under the lock every writer of that signal takes.
#[derive(Debug)]
pub(super) struct Linked {
    pub(super) token: Weak<SubtreeToken>,
    pub(super) shared: Arc<SubtreeShared>,
    /// What trapping exit signals adds for this actor; `None` for an actor
    /// that adds nothing by it (any actor but a worker below a supervisor
    /// whose `Shutdown` has a window or is `Infinity`).
    pub(super) trap: Option<Box<TrapSpec>>,
}

/// The token of one spawned actor. Held by the actor's [`SubtreeHold`] and
/// by the tokens of the actors it spawned.
#[derive(Debug)]
pub(crate) struct SubtreeToken {
    pub(super) shared: Arc<SubtreeShared>,
    /// The time this actor can outlive a killed parent, in nanoseconds. It
    /// rises once, when a worker starts to trap exit signals (see
    /// `TrapSpec`), and never falls.
    pub(super) own_nanos: AtomicU64,
    pub(super) parent: Option<Arc<SubtreeToken>>,
}

/// What a worker that traps exit signals adds to the budget above it. Kept
/// beside the actor's record, which only a worker below a supervisor whose
/// `Shutdown` has a window or is `Infinity` makes one for.
#[derive(Debug)]
pub(super) struct TrapSpec {
    /// What the worker adds to `own_nanos`: its own `Shutdown` window, since
    /// a killed supervisor then stops it in order within that window instead
    /// of killing it. Zero for a worker whose stop has no window to add.
    pub(super) extra_nanos: u64,
    /// Whether the actor is a worker under `Shutdown::Infinity`: once it
    /// traps exit signals its orderly stop has no bound, so it counts among
    /// the live unbounded stops above it, as a supervisor child under
    /// `Infinity` does from its start.
    pub(super) infinity: bool,
    /// The count `infinity` raised, held until the actor's hold drops.
    pub(super) mark: Mutex<TrapMark>,
    /// Set by the first trap: the budget rises once per incarnation, so a
    /// later `trap_exit`, or one after `untrap_exit`, raises nothing more.
    pub(super) raised: AtomicBool,
}

/// The state of the unbounded-stop count a trapping worker adds.
#[derive(Debug, Default)]
pub(super) enum TrapMark {
    /// The actor has not trapped.
    #[default]
    Unset,
    /// The actor trapped: the count is raised until the mark drops.
    Held(#[allow(dead_code)] InfinityMark),
    /// The actor's hold dropped: the count was given back, and a later trap
    /// raises nothing.
    Closed,
}

impl SubtreeToken {
    /// Raises the budget below this token to at least `candidate`, and the
    /// parent's the same way.
    pub(super) fn raise_below(&self, candidate: u64) {
        let previous = self
            .shared
            .below_nanos
            .fetch_max(candidate, Ordering::SeqCst);
        if candidate > previous {
            self.shared
                .modify(|s| s.below_gen = s.below_gen.wrapping_add(1));
            if let Some(parent) = &self.parent {
                parent.raise_below(
                    self.own_nanos
                        .load(Ordering::SeqCst)
                        .saturating_add(candidate),
                );
            }
        }
    }

    /// The actor started to trap exit signals: a killed parent stops it in
    /// order within its own window, so the budget of every wait above it
    /// rises by that window. Once; a later `untrap_exit` lowers nothing, as
    /// the budget never falls.
    pub(super) fn raise_for_trap(&self, spec: &TrapSpec) {
        if spec.raised.swap(true, Ordering::SeqCst) {
            return;
        }
        if spec.infinity {
            self.mark_unbounded_stop(spec);
        }
        if spec.extra_nanos == 0 {
            return;
        }
        let base = self.own_nanos.load(Ordering::SeqCst);
        let raised = base.saturating_add(spec.extra_nanos);
        let previous = self.own_nanos.fetch_max(raised, Ordering::SeqCst);
        if raised > previous {
            if let Some(parent) = &self.parent {
                let below = self.shared.below_nanos.load(Ordering::SeqCst);
                parent.raise_below(raised.saturating_add(below));
            }
        }
    }

    /// Counts this worker among the live unbounded stops above it, once, for
    /// as long as its hold lives. A later `untrap_exit` lowers nothing, as
    /// the budget never falls.
    pub(super) fn mark_unbounded_stop(&self, spec: &TrapSpec) {
        let Some(parent) = &self.parent else {
            return;
        };
        let mut mark = spec.mark.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(*mark, TrapMark::Unset) {
            SubtreeToken::shift_infinity(parent, true);
            *mark = TrapMark::Held(InfinityMark {
                above: parent.clone(),
            });
        }
    }

    /// Gives the count back when the actor's hold drops.
    pub(super) fn close_trap_mark(spec: &TrapSpec) {
        let released = {
            let mut mark = spec.mark.lock().unwrap_or_else(|p| p.into_inner());
            std::mem::replace(&mut *mark, TrapMark::Closed)
        };
        drop(released);
    }

    /// Raises or lowers by one the live-supervisor count of `token` and of
    /// every ancestor.
    pub(super) fn shift_infinity(token: &Arc<SubtreeToken>, raise: bool) {
        let mut at = Some(token);
        while let Some(current) = at {
            current.shared.modify(|s| {
                s.inf_live = if raise {
                    s.inf_live.saturating_add(1)
                } else {
                    s.inf_live.saturating_sub(1)
                };
                s.inf_gen = s.inf_gen.wrapping_add(1);
            });
            at = current.parent.as_ref();
        }
    }
}

impl Drop for SubtreeToken {
    fn drop(&mut self) {
        self.shared.modify(|s| s.gone = true);
    }
}

/// How a child's token attaches to its parent's.
pub(crate) struct SubtreeLink {
    pub parent: Arc<SubtreeToken>,
    /// The time the child can outlive a killed parent. A value beyond what a
    /// `u64` of nanoseconds holds reads as unbounded.
    pub own: Duration,
    /// Whether the child is a supervisor whose orderly stop has no bound.
    pub infinity: bool,
    /// What the child adds to `own` once its actor traps exit signals: its
    /// own `Shutdown` window for a worker, zero for a supervisor child.
    pub trap_extra: Duration,
    /// Whether the child is a worker under `Shutdown::Infinity`, whose
    /// orderly stop has no bound once its actor traps exit signals.
    pub trap_infinity: bool,
}

/// Lowers the live-supervisor count of the ancestors of an unbounded
/// supervisor child when the child's actor ends, whether or not anything
/// below it has.
#[derive(Debug)]
pub(super) struct InfinityMark {
    pub(super) above: Arc<SubtreeToken>,
}

impl Drop for InfinityMark {
    fn drop(&mut self) {
        SubtreeToken::shift_infinity(&self.above, false);
    }
}

/// What an actor holds of its token for as long as it runs: its context and
/// its registry guard share one.
#[derive(Debug)]
pub(crate) struct SubtreeHold {
    pub(super) token: Arc<SubtreeToken>,
    pub(super) _mark: Option<InfinityMark>,
    /// The abort record of the actor's incarnation, released when the hold
    /// drops: the actor ended, so no abort is owed to it any more.
    pub(super) record: IncarnationRef,
    /// Declared last, so the hold's drop ends with it: it says the hold has
    /// been released once the hold's reference to the token is gone.
    pub(super) _released: ReleaseSignal,
}

/// The last thing the drop of a [`SubtreeHold`] does: marks the hold released
/// and wakes the waits on the subtree. Fields drop in declaration order after
/// the hold's own `drop`, so this runs after the hold's reference to the
/// token dropped. A hold with nothing below it therefore reads released and
/// the subtree gone together, and a reader of the subtree that finds the hold
/// released and the subtree not gone knows that something else still holds
/// the token: an actor below, or a restart of one that is not dropped yet. No
/// reader of the abort chain is such a holder: the chain is read under the
/// lock the hold's release takes before it lets go of the token.
#[derive(Debug)]
pub(super) struct ReleaseSignal(Arc<SubtreeShared>);

impl Drop for ReleaseSignal {
    fn drop(&mut self) {
        #[cfg(test)]
        test_hook::run_before_release();
        self.0.released.store(true, Ordering::SeqCst);
        // An observer's waker belongs to the observer's task: a panic from it
        // must not unwind into the drop that releases the hold.
        self.0.wake_waits("subtree release");
    }
}

impl Drop for SubtreeHold {
    fn drop(&mut self) {
        self.record.close_trap_mark();
        self.record.release();
        if let Some(mode) = self.record.mode() {
            mode.holds.dropped();
        }
        #[cfg(test)]
        test_hook::run_after_release();
    }
}

impl SubtreeHold {
    /// The hold of a new actor whose stop lane carries `record`: the record
    /// is linked to the actor's token, so its aborts reach the subtree's
    /// waits.
    pub(crate) fn with_record(link: Option<SubtreeLink>, record: IncarnationRef) -> Arc<Self> {
        let trap = link.as_ref().and_then(|link| {
            let extra_nanos = rules::saturating_nanos(link.trap_extra);
            (extra_nanos != 0 || link.trap_infinity).then(|| {
                Box::new(TrapSpec {
                    extra_nanos,
                    infinity: link.trap_infinity,
                    mark: Mutex::new(TrapMark::Unset),
                    raised: AtomicBool::new(false),
                })
            })
        });
        if let Some(mode) = record.mode() {
            mode.holds.built();
        }
        let hold = Self::build(link, record);
        hold.record.link(&hold.token, trap);
        hold
    }

    /// The hold of a new actor: a root, or the child described by `link`.
    #[cfg(test)]
    pub(crate) fn new(link: Option<SubtreeLink>) -> Arc<Self> {
        Self::build(link, IncarnationRef::new())
    }

    pub(super) fn build(link: Option<SubtreeLink>, record: IncarnationRef) -> Arc<Self> {
        let (token, mark) = match link {
            None => (
                Arc::new(SubtreeToken {
                    shared: SubtreeShared::new(),
                    own_nanos: AtomicU64::new(0),
                    parent: None,
                }),
                None,
            ),
            Some(link) => {
                let own_nanos = rules::saturating_nanos(link.own);
                let token = Arc::new(SubtreeToken {
                    shared: SubtreeShared::new(),
                    own_nanos: AtomicU64::new(own_nanos),
                    parent: Some(link.parent.clone()),
                });
                link.parent.raise_below(own_nanos);
                let mark = link.infinity.then(|| {
                    SubtreeToken::shift_infinity(&link.parent, true);
                    InfinityMark {
                        above: link.parent.clone(),
                    }
                });
                (token, mark)
            }
        };
        let released = ReleaseSignal(token.shared.clone());
        Arc::new(Self {
            token,
            _mark: mark,
            record,
            _released: released,
        })
    }

    /// The token the actor's own children attach to.
    pub(crate) fn token(&self) -> Arc<SubtreeToken> {
        self.token.clone()
    }

    /// An observer of the actor's subtree that never keeps it alive.
    pub(crate) fn watch(&self) -> SubtreeWatch {
        SubtreeWatch {
            shared: self.token.shared.clone(),
            observer: false,
        }
    }
}

/// An observer of one actor's subtree. Cloning it is cheap, and holding it
/// never keeps the subtree alive.
///
/// Its waits park on the library's list of the token, or, once
/// [`for_observer`](Self::for_observer) made it a subtree observer's, on the
/// observers' list (see the module note).
#[derive(Clone, Debug)]
pub(crate) struct SubtreeWatch {
    pub(super) shared: Arc<SubtreeShared>,
    /// Whether the watch's waits are a subtree observer's, whose task is a
    /// user's.
    pub(super) observer: bool,
}

impl SubtreeWatch {
    /// This watch for a subtree observer's waits: they park on the observers'
    /// list, where a panic of a user's waker can strand only other observers.
    pub(crate) fn for_observer(mut self) -> Self {
        self.observer = true;
        self
    }

    /// Whether the actor and everything below it ended.
    pub(crate) fn is_gone(&self) -> bool {
        self.shared.signal().gone
    }

    /// Whether the actor's own hold has been released: the actor let go of
    /// what it held, and the actors below it may still run.
    #[cfg(test)]
    pub(crate) fn is_released(&self) -> bool {
        self.shared.released.load(Ordering::SeqCst)
    }

    /// Resolves once the actor's own hold has been released, at once when it
    /// was, whatever runs below the actor. It reads no clock and needs no
    /// timer, so a caller on any runtime, or none, can wait through it. When
    /// it resolves and the subtree is not gone, something else holds the
    /// token: an actor below the observed one, or a restart of one that is
    /// not dropped yet.
    pub(crate) async fn released(&self) {
        loop {
            // Created before the read: a release after the read wakes this
            // wait.
            let notified = self.shared.notified(self.observer);
            tokio::pin!(notified);
            notified.as_mut().enable();
            let released = self.shared.released.load(Ordering::SeqCst);
            #[cfg(test)]
            test_hook::run();
            if released {
                return;
            }
            notified.await;
        }
    }

    /// The live-supervisor count below the actor.
    pub(super) fn infinity_live(&self) -> u32 {
        self.shared.signal().inf_live
    }

    /// The bound of a wait on the actor's subtree that starts now: `None`
    /// while a supervisor child with an unbounded orderly stop is alive below
    /// it (or when the budget saturated), else the budget plus the settle
    /// time, capped like every wait.
    pub(crate) fn release_bound(&self) -> Option<Duration> {
        rules::successor_bound(
            self.shared.below_nanos.load(Ordering::SeqCst),
            self.infinity_live(),
        )
        .map(bounded_wait)
    }

    /// The latest abort issued against an actor of the subtree, and whether
    /// a live actor of it is owed an abort not yet issued.
    pub(crate) fn abort_view(&self) -> (Option<Instant>, bool) {
        let aborts = self.shared.aborts();
        (aborts.latest, aborts.owed > 0)
    }
}

#[cfg(test)]
impl SubtreeWatch {
    /// Resolves once the actor and everything below it ended, at once when
    /// they did.
    pub(crate) async fn gone(&self) {
        self.wait_until(|s| s.gone).await;
    }

    /// Resolves once no supervisor child with an unbounded orderly stop is
    /// alive below the actor, at once when none is.
    pub(crate) async fn infinity_cleared(&self) {
        self.wait_until(|s| s.inf_live == 0).await;
    }

    pub(super) async fn wait_until(&self, done: impl Fn(&SubtreeSignal) -> bool) {
        loop {
            let notified = self.shared.notified(self.observer);
            tokio::pin!(notified);
            notified.as_mut().enable();
            if done(&self.shared.signal()) {
                return;
            }
            notified.await;
        }
    }
}

/// A wait on a subtree, with the deadline it expires at.
///
/// The deadline is armed from the moment the wait has a finite bound, so a
/// subtree with an unbounded supervisor child below is waited for without
/// one, and the bound counts from the moment the last such child ended. A
/// new unbounded supervisor child disarms the deadline again, and a raise of
/// the budget extends it to the later of the armed deadline and the raise's
/// moment plus the new bound.
pub(crate) struct SubtreeWait {
    pub(super) watch: SubtreeWatch,
    /// The signal as this wait last read it.
    pub(super) seen: SubtreeSignal,
    pub(super) deadline: Option<Instant>,
    /// The bound the deadline was last armed with: the cap of a wait for an
    /// owed abort counts it once more from the deadline.
    pub(super) bound: Duration,
    /// The budget read when the deadline was last armed: a wake on the
    /// budget's signal that finds it unchanged came from the subtree's
    /// aborts and arms nothing.
    pub(super) below_seen: u64,
}

/// What woke a [`SubtreeWait`].
pub(super) enum WaitWake {
    Below,
    Deadline,
}

impl SubtreeWait {
    /// Starts a wait on `watch`, arming its deadline when it has a bound.
    pub(crate) fn new(watch: &SubtreeWatch) -> Self {
        let seen = watch.shared.signal();
        let mut wait = Self {
            watch: watch.clone(),
            seen,
            deadline: None,
            bound: Duration::ZERO,
            below_seen: 0,
        };
        wait.rearm();
        wait
    }

    /// Whether the subtree ended.
    pub(crate) fn gone(&self) -> bool {
        self.watch.is_gone()
    }

    /// Whether the wait may give its verdict now: the armed deadline passed,
    /// no live actor of the subtree is owed an abort not yet issued (up to
    /// the cap), and the latest abort issued against one of them is at least
    /// the settle time old ([`rules::verdict_floor`]).
    pub(crate) fn expired(&self) -> bool {
        self.verdict_at().is_some_and(|at| Instant::now() >= at)
    }

    /// The instant the verdict may be given, as the subtree reads now.
    pub(super) fn verdict_at(&self) -> Option<Instant> {
        let deadline = self.deadline?;
        let (latest, owed) = self.watch.abort_view();
        Some(Instant::from_std(rules::verdict_floor(
            deadline.into_std(),
            self.bound,
            latest.map(Instant::into_std),
            owed,
        )))
    }

    #[cfg(test)]
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Re-reads the bound: unbounded disarms the deadline, a finite bound arms
    /// it from now or extends it, never shortens it.
    pub(super) fn rearm(&mut self) {
        self.below_seen = self.watch.shared.below_nanos.load(Ordering::SeqCst);
        match self.watch.release_bound() {
            None => self.deadline = None,
            Some(bound) => {
                let at = saturating_deadline(Instant::now(), bound);
                self.deadline = Some(self.deadline.map_or(at, |armed| armed.max(at)));
                self.bound = self.bound.max(bound);
            }
        }
    }

    /// A wake on the budget's signal: arms again only when the budget rose,
    /// since the subtree's aborts share the signal.
    pub(super) fn rearm_if_raised(&mut self) {
        if self.watch.shared.below_nanos.load(Ordering::SeqCst) != self.below_seen {
            self.rearm();
        }
    }

    /// Resolves at the next change that can decide the wait: the subtree
    /// ending, the count of unbounded supervisor children or the budget
    /// changing (after which the deadline is re-read), a change of the
    /// subtree's aborts, or the instant the verdict may be given.
    pub(crate) async fn changed(&mut self) {
        let shared = self.watch.shared.clone();
        // Created before the read: a change after the read wakes this wait.
        let notified = shared.notified(self.watch.observer);
        tokio::pin!(notified);
        notified.as_mut().enable();
        let current = shared.signal();
        if current.gone {
            return;
        }
        // A change since this wait last read the signal is handled at once.
        let wake = if current != self.seen {
            WaitWake::Below
        } else {
            let deadline = self.verdict_at();
            tokio::select! {
                biased;
                () = &mut notified => WaitWake::Below,
                _ = sleep_until_opt(deadline) => WaitWake::Deadline,
            }
        };
        if let WaitWake::Below = wake {
            let now = shared.signal();
            let was = std::mem::replace(&mut self.seen, now);
            if now.gone {
                return;
            }
            if now.inf_gen != was.inf_gen {
                self.rearm();
            } else if now.below_gen != was.below_gen {
                self.rearm_if_raised();
            }
        }
    }
}

pub(super) async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

impl IncarnationRef {
    /// Links the record to the incarnation's subtree token.
    pub(super) fn link(&self, token: &Arc<SubtreeToken>, trap: Option<Box<TrapSpec>>) {
        let _ = self.shared.linked.set(Linked {
            token: Arc::downgrade(token),
            shared: token.shared.clone(),
            trap,
        });
    }

    /// The `Shutdown` of a worker below a supervisor, as its trap spec stands
    /// for it: the window of a `Timeout`, `Infinity`, else `Kill`. What a
    /// link guard needs to know of a child that traps exit signals when the
    /// guard fires.
    pub(crate) fn worker_shutdown(&self) -> crate::types::Shutdown {
        match self.shared.linked.get().and_then(|l| l.trap.as_deref()) {
            None => crate::types::Shutdown::Kill,
            Some(spec) if spec.infinity => crate::types::Shutdown::Infinity,
            Some(spec) => {
                crate::types::Shutdown::Timeout(std::time::Duration::from_nanos(spec.extra_nanos))
            }
        }
    }

    /// Gives back the count the trap of a worker under `Shutdown::Infinity`
    /// raised, when the actor's hold drops.
    pub(super) fn close_trap_mark(&self) {
        if let Some(spec) = self.shared.linked.get().and_then(|l| l.trap.as_deref()) {
            SubtreeToken::close_trap_mark(spec);
        }
    }

    /// An observer of the incarnation's subtree, or `None` while the record
    /// is not linked, which is before the actor's hold is built (a spawn
    /// registers the actor first) and for a record that never got one.
    ///
    /// The link is set before the actor's task is spawned and a task cannot
    /// end before it is spawned, so a reader that has seen the actor end
    /// finds the link unless the actor never ran. The answer to "has the
    /// subtree ended" is read from the signal under its lock, never inferred
    /// from a failed upgrade of the token.
    pub(crate) fn subtree_watch(&self) -> Option<SubtreeWatch> {
        self.shared.linked.get().map(|linked| SubtreeWatch {
            shared: linked.shared.clone(),
            observer: false,
        })
    }
}

#[cfg(test)]
mod abort_record_tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A root and one child below it, the child's record linked to its
    /// token, the child's task held by the test.
    fn child_of(root: &Arc<SubtreeHold>) -> (Arc<SubtreeHold>, IncarnationRef) {
        let record = IncarnationRef::new();
        let link = SubtreeLink {
            parent: root.token(),
            own: ms(200),
            infinity: false,
            trap_extra: std::time::Duration::ZERO,
            trap_infinity: false,
        };
        (SubtreeHold::with_record(Some(link), record.clone()), record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(parent: &Arc<SubtreeHold>, own: Duration, infinity: bool) -> Arc<SubtreeHold> {
        SubtreeHold::new(Some(SubtreeLink {
            parent: parent.token(),
            own,
            infinity,
            trap_extra: std::time::Duration::ZERO,
            trap_infinity: false,
        }))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// An actor whose value takes a while to drop and says when it is done.
    struct Slow {
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for Slow {
        fn drop(&mut self) {
            std::thread::sleep(ms(150));
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl crate::actor::Actor for Slow {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    /// A message whose value takes a while to drop.
    struct SlowMsg;

    impl Drop for SlowMsg {
        fn drop(&mut self) {
            std::thread::sleep(ms(150));
        }
    }

    /// An actor that holds a name and a mailbox.
    struct Named;

    impl crate::actor::Actor for Named {
        type Message = SlowMsg;
        type Response = ();

        async fn handle(
            &mut self,
            _msg: SlowMsg,
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    /// An actor that parks in `on_started` and drops slowly.
    struct Parked {
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for Parked {
        fn drop(&mut self) {
            std::thread::sleep(ms(150));
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl crate::actor::Actor for Parked {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }

        async fn on_started(
            &mut self,
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    /// Polls `watch` from a thread of its own until the subtree reads as
    /// ended, then returns `read()` taken at that moment. The thread runs no
    /// task of the runtime, so its answer cannot wait behind a dropping task.
    fn read_at_the_end<T: Send + 'static>(
        watch: SubtreeWatch,
        read: impl FnOnce() -> T + Send + 'static,
    ) -> std::thread::JoinHandle<T> {
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            while !watch.is_gone() {
                assert!(started.elapsed() < Duration::from_secs(5), "never ended");
                std::thread::sleep(Duration::from_millis(1));
            }
            read()
        })
    }

    /// An actor whose value takes a while to drop, or panics when dropped.
    struct DropProbe {
        dropped: Arc<std::sync::atomic::AtomicBool>,
        panics: bool,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            std::thread::sleep(ms(150));
            self.dropped.store(true, Ordering::SeqCst);
            if self.panics {
                panic!("the actor value panicked when dropped");
            }
        }
    }

    impl crate::actor::Actor for DropProbe {
        type Message = ();
        type Response = ();

        async fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut crate::actor::context::ActorContext<Self>,
        ) -> crate::error::ActorResult<()> {
            Ok(())
        }
    }

    fn spawn_drop_probe(
        panics: bool,
    ) -> (
        Arc<std::sync::atomic::AtomicBool>,
        crate::actor::handle::ActorHandle<DropProbe>,
        tokio::task::JoinHandle<crate::types::StopReason>,
    ) {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, join, _watch) = crate::actor::runtime::spawn_actor(
            crate::types::ActorId::from(format!("drop-probe-{}", uuid::Uuid::new_v4())),
            DropProbe {
                dropped: dropped.clone(),
                panics,
            },
            crate::actor::runtime::ActorConfig::default(),
            None,
            None,
            None,
            true,
            Duration::from_millis(120),
            None,
        )
        .expect("the actor spawns");
        (dropped, handle, join)
    }

    /// `Stopped` is published after the actor value, and the code its `Drop`
    /// runs, are gone.
    async fn stopped_means_the_actor_value_is_dropped() {
        let (dropped, handle, join) = spawn_drop_probe(false);
        handle
            .stop(crate::types::StopReason::Graceful)
            .await
            .expect("the stop is accepted");
        handle.wait_stopped().await;
        assert!(
            dropped.load(Ordering::SeqCst),
            "Stopped was observed before the actor value was dropped"
        );
        assert_eq!(
            handle.status(),
            crate::types::ActorStatus::Stopped,
            "the status reads Stopped"
        );
        let reason = join.await.expect("the task ends");
        assert!(
            matches!(reason, crate::types::StopReason::Graceful),
            "{reason:?}"
        );
    }

    /// A panic in the actor value's `Drop` comes after the reason is final:
    /// the actor ends in exactly one reason, the one it was stopped with, on
    /// its plane as in its task's return, `Stopped` is still published once
    /// the value is gone, and with the `tracing` feature the panic is logged
    /// once with the actor's id.
    async fn a_panicking_actor_drop_ends_in_one_reason() {
        #[cfg(feature = "tracing")]
        let logged = crate::actor::supervision::lost_runner::capture::install();
        for request in [
            crate::types::StopReason::Graceful,
            crate::types::StopReason::Failure(crate::error::ActorError::user("earlier")),
            crate::types::StopReason::Kill,
        ] {
            let (dropped, handle, join) = spawn_drop_probe(true);
            let status = handle.status_rx();
            handle
                .stop(request.clone())
                .await
                .expect("the stop is accepted");
            let reason = join.await.expect("the task returns its reason");
            assert!(dropped.load(Ordering::SeqCst));
            assert_eq!(reason.to_string(), request.to_string(), "the reason stands");
            assert_eq!(
                status.borrow().reason.as_ref().map(ToString::to_string),
                Some(request.to_string()),
                "the plane reads the same reason"
            );
            assert_eq!(
                status.borrow().status,
                crate::types::ActorStatus::Stopped,
                "the status reads Stopped"
            );
            #[cfg(feature = "tracing")]
            {
                let token = format!("actor_id={} ", handle.id());
                let lines: Vec<String> = logged
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|line| line.contains(&token) && line.contains("the reason stands"))
                    .cloned()
                    .collect();
                assert_eq!(lines.len(), 1, "one warning under {request:?}: {lines:?}");
            }
        }
    }
}

/// A wait on a subtree gives its verdict no later than its deadline plus its
/// bound plus the settle time, whatever its members do near that cap: an
/// end or an abort recorded within a settle time of the last one raises the
/// floor no further than the ceiling.
#[cfg(test)]
mod ceiling_tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A child below `parent` with `own` budget, its record linked to its
    /// token.
    fn below(parent: &Arc<SubtreeHold>, own: Duration) -> (Arc<SubtreeHold>, IncarnationRef) {
        let record = IncarnationRef::new();
        let hold = SubtreeHold::with_record(
            Some(SubtreeLink {
                parent: parent.token(),
                own,
                infinity: false,
                trap_extra: std::time::Duration::ZERO,
                trap_infinity: false,
            }),
            record.clone(),
        );
        (hold, record)
    }

    /// Runs the wait 1 ms at a time from now, applying `step(offset)` at
    /// each tick before reading it, and returns the offset of the first tick
    /// the wait reads expired.
    async fn first_verdict(wait: &SubtreeWait, mut step: impl FnMut(u64)) -> u64 {
        for offset in 0..5_000 {
            step(offset);
            if wait.expired() {
                return offset;
            }
            tokio::time::advance(ms(1)).await;
        }
        panic!("no verdict in 5 s");
    }

    /// A root P owed an abort, a wait on its subtree, and the clock moved to
    /// one millisecond before the wait's cap. Returns the wait, its
    /// deadline and its bound.
    async fn owed_root_at_the_cap(
        p: &Arc<SubtreeHold>,
        p_rec: &IncarnationRef,
    ) -> (SubtreeWait, Instant, Duration) {
        let wait = SubtreeWait::new(&p.watch());
        let start = Instant::now();
        let deadline = wait.deadline().unwrap();
        let bound = deadline - start;
        p_rec.owe();
        tokio::time::advance(bound + bound - ms(1)).await;
        (wait, deadline, bound)
    }
}

/// A one-shot hook run by the calling thread right after the next read of a
/// subtree signal or an abort record, so a test can land a change exactly
/// between a read and the wait that follows it.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::cell::RefCell;

    type Hook = RefCell<Option<Box<dyn FnOnce()>>>;

    thread_local! {
        static HOOK: Hook = const { RefCell::new(None) };
        static AFTER_WRITE: Hook = const { RefCell::new(None) };
        static BEFORE_PUSH: Hook = const { RefCell::new(None) };
        static BEFORE_RELEASE: Hook = const { RefCell::new(None) };
        static IN_CHAIN: Hook = const { RefCell::new(None) };
        static AFTER_RELEASE: Hook = const { RefCell::new(None) };
    }

    pub(crate) fn arm_in_chain(hook: impl FnOnce() + 'static) {
        IN_CHAIN.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn run_in_chain() {
        if let Some(hook) = IN_CHAIN.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    pub(crate) fn arm_after_release(hook: impl FnOnce() + 'static) {
        AFTER_RELEASE.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn run_after_release() {
        if let Some(hook) = AFTER_RELEASE.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    pub(crate) fn arm(hook: impl FnOnce() + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn run() {
        if let Some(hook) = HOOK.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    /// Armed like [`arm`], but run by the next first-abort record right after
    /// it wrote the abort's instant and before it wakes the waits.
    pub(crate) fn arm_after_write(hook: impl FnOnce() + 'static) {
        AFTER_WRITE.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    /// Armed like [`arm`], but run by the next first-abort record while it
    /// holds the owed lock, after it wrote the instant and before it queues
    /// the abort's event.
    pub(crate) fn arm_before_push(hook: impl FnOnce() + 'static) {
        BEFORE_PUSH.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn run_before_push() {
        if let Some(hook) = BEFORE_PUSH.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    pub(crate) fn run_after_write() {
        if let Some(hook) = AFTER_WRITE.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    /// Armed like [`arm`], but run by the next hold's release right before it
    /// writes that the hold is released.
    pub(crate) fn arm_before_release(hook: impl FnOnce() + 'static) {
        BEFORE_RELEASE.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    pub(crate) fn run_before_release() {
        if let Some(hook) = BEFORE_RELEASE.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }
}

#[cfg(test)]
mod lost_wake_tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn child(parent: &Arc<SubtreeHold>, own: Duration, infinity: bool) -> Arc<SubtreeHold> {
        SubtreeHold::new(Some(SubtreeLink {
            parent: parent.token(),
            own,
            infinity,
            trap_extra: std::time::Duration::ZERO,
            trap_infinity: false,
        }))
    }
}

/// A worker under `Shutdown::Infinity` that traps exit signals is waited for
/// without a bound, as a supervisor child under `Infinity` is, for as long as
/// its actor lives.
#[cfg(test)]
mod trap_unbounded_tests {
    use super::*;
    use crate::types::Shutdown;

    fn worker_below(
        parent: &Arc<SubtreeHold>,
        trap_infinity: bool,
    ) -> (Arc<SubtreeHold>, IncarnationRef) {
        let record = IncarnationRef::new();
        let hold = SubtreeHold::with_record(
            Some(SubtreeLink {
                parent: parent.token(),
                own: Duration::from_millis(50),
                infinity: false,
                trap_extra: Duration::ZERO,
                trap_infinity,
            }),
            record.clone(),
        );
        (hold, record)
    }

    #[test]
    fn a_trapping_worker_under_infinity_holds_the_wait_until_its_actor_ends() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let (worker, record) = worker_below(&root, true);
        assert_eq!(watch.infinity_live(), 0, "not trapping: bounded");
        assert!(watch.release_bound().is_some());
        record.set_trap(true);
        assert_eq!(watch.infinity_live(), 1, "trapping: counted once");
        assert!(watch.release_bound().is_none(), "and without a bound");
        record.set_trap(true);
        record.set_trap(false);
        assert_eq!(
            watch.infinity_live(),
            1,
            "a toggle counts nothing more and takes nothing back"
        );
        // A child below the worker keeps the worker's token alive after the
        // worker's actor ended; the count is the actor's, not the token's.
        let (kid, _kid_record) = worker_below(&worker, false);
        drop(worker);
        assert_eq!(watch.infinity_live(), 0, "the actor's end gives it back");
        assert!(watch.release_bound().is_some());
        record.set_trap(true);
        assert_eq!(
            watch.infinity_live(),
            0,
            "a trap after the end counts nothing"
        );
        drop(kid);
    }

    /// A link built the way a spawn builds it, from the child's kind and
    /// `Shutdown`.
    fn link_below(
        parent: &Arc<SubtreeHold>,
        is_supervisor: bool,
        shutdown: Shutdown,
        record: &IncarnationRef,
    ) -> Arc<SubtreeHold> {
        SubtreeHold::with_record(
            Some(SubtreeLink {
                parent: parent.token(),
                own: Duration::from_millis(50),
                infinity: is_supervisor && shutdown == Shutdown::Infinity,
                trap_extra: rules::trap_extra(is_supervisor, shutdown),
                trap_infinity: rules::trap_infinity(is_supervisor, shutdown),
            }),
            record.clone(),
        )
    }

    /// A supervisor child under `Infinity` is counted from its start, and
    /// trapping adds nothing: it is one unbounded stop, not two.
    #[test]
    fn a_trapping_supervisor_child_under_infinity_is_counted_once() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let record = IncarnationRef::new();
        let _child = link_below(&root, true, Shutdown::Infinity, &record);
        assert_eq!(watch.infinity_live(), 1, "counted from its start");
        record.set_trap(true);
        assert_eq!(watch.infinity_live(), 1, "trapping counts nothing more");
    }

    /// A worker is counted by trapping only under `Infinity`: under a window
    /// or `Kill` it never enters the count.
    #[test]
    fn a_worker_is_counted_by_trapping_only_under_infinity() {
        for (shutdown, counted) in [
            (Shutdown::Kill, false),
            (Shutdown::Timeout(Duration::from_secs(3)), false),
            (Shutdown::Infinity, true),
        ] {
            let root = SubtreeHold::new(None);
            let watch = root.watch();
            let record = IncarnationRef::new();
            let _worker = link_below(&root, false, shutdown, &record);
            assert_eq!(watch.infinity_live(), 0, "{shutdown:?}: not trapping");
            record.set_trap(true);
            assert_eq!(
                watch.infinity_live(),
                u32::from(counted),
                "{shutdown:?}: trapping"
            );
        }
    }

    /// A worker with a window adds the window to the budget above it once it
    /// traps, and not before, and only once per incarnation.
    #[test]
    fn a_trapping_worker_with_a_window_raises_the_budget_above_it() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let record = IncarnationRef::new();
        let window = Duration::from_secs(3);
        let _worker = link_below(&root, false, Shutdown::Timeout(window), &record);
        let before = watch.release_bound().expect("a bound");
        record.set_trap(true);
        let after = watch.release_bound().expect("a bound");
        assert!(
            after >= before + window,
            "the window is added once the worker traps: {before:?} then {after:?}"
        );
        record.set_trap(true);
        record.set_trap(false);
        record.set_trap(true);
        assert_eq!(
            watch.release_bound().expect("a bound"),
            after,
            "a second trap, and one after untrapping, raise nothing more"
        );
    }
}

/// The size of the per-incarnation record, pinned: it holds the system's
/// telemetry state by one pointer and nothing else for telemetry. The record
/// is no larger than a mirror of its fields as they were before the pointer,
/// built from the same field types in the same build, so the claim holds on
/// every target and toolchain; the exact sizes are pinned only where they were
/// measured.
#[cfg(test)]
mod footprint_tests {
    use super::*;

    /// The owed state with the mark held inline.
    #[allow(dead_code)]
    struct InlineOwed {
        at: Option<Instant>,
        mark: Option<InlineMark>,
        ended: bool,
    }

    #[allow(dead_code)]
    struct InlineMark {
        chain: Vec<Arc<SubtreeShared>>,
    }

    /// The record's fields without the pointer to the telemetry state, with
    /// the owed state held inline.
    #[allow(dead_code)]
    struct WithoutPointer {
        linked: OnceLock<Linked>,
        owed: Mutex<InlineOwed>,
        status: crate::actor::incarnation::status_cell::StatusCell,
        recorded: tokio::sync::Notify,
        reader: crate::actor::incarnation::WakeSlot,
        lane: crate::actor::incarnation::LaneCell,
        signals: tokio::sync::Notify,
        system: Mutex<SystemSlot>,
        system_state: std::sync::atomic::AtomicU8,
    }

    /// The sizes as measured on x86_64 Linux, where the standard library's
    /// locks and the layout of the fields are the ones measured.
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    #[test]
    fn the_record_has_its_measured_size() {
        // The block's own pieces (the incarnation number and the signal
        // cell) are what it holds beyond v0.12.0's 408 bytes; the telemetry
        // pointer and the three telemetry atomics (the latter in the tail of
        // the lane's cell) add nothing.
        assert_eq!(
            std::mem::size_of::<IncarnationBlock>(),
            416,
            "IncarnationBlock"
        );
        assert_eq!(
            std::mem::size_of::<crate::actor::incarnation::LaneCell>(),
            96,
            "the lane's cell, with the record's three atomics in its tail"
        );
        assert_eq!(std::mem::size_of::<Owed>(), 32, "Owed");
        assert_eq!(
            std::mem::size_of::<OnceLock<Arc<crate::telemetry::Mode>>>(),
            16,
            "the record's pointer to the system's telemetry state"
        );
    }
}

#[cfg(test)]
#[path = "subtree_system_tests.rs"]
mod system_tests;

#[cfg(test)]
#[path = "subtree_waker_tests.rs"]
mod waker_tests;
