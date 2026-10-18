//! Subtree tokens: how a restart learns that every actor its previous
//! incarnation spawned has ended.
//!
//! Every spawned actor holds a token. A token is held by the actor's run
//! loop, its context, its registry guard and its timer and stream
//! forwarders, and by the token of each actor it spawned as their parent, so
//! a token is dropped only when the actor, everything it owned and everything
//! below it have ended. Its drop glue signals `gone`, and drop glue runs when
//! a task is aborted too, so the signal fires on every way an actor can end.
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
//! that was owed an abort counts as that abort.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::{sleep_until, Instant};

use crate::actor::runtime::{bounded_wait, saturating_deadline};
use crate::actor::supervision::rules;

/// The state a token and every watch of it share.
#[derive(Debug)]
pub(crate) struct SubtreeShared {
    /// Set when the token is dropped: the actor and everything below it ended.
    gone: watch::Sender<bool>,
    /// The budget of the subtree below this token, in nanoseconds. A value of
    /// `u64::MAX` is unbounded.
    below_nanos: AtomicU64,
    /// The number of live supervisor children below this token whose orderly
    /// stop has no bound.
    inf_live: watch::Sender<u32>,
    /// Signalled whenever `below_nanos` is raised, and whenever `aborts`
    /// changes.
    below_changed: watch::Sender<()>,
    /// The aborts owed to and issued against the live actors of the subtree.
    aborts: Mutex<SubtreeAborts>,
}

/// What a subtree's aborts say about a verdict on it.
#[derive(Debug, Default, Clone, Copy)]
struct SubtreeAborts {
    /// Live actors of the subtree, its root included, that are owed an abort
    /// none has issued yet.
    owed: u32,
    /// The latest first abort issued against an actor of the subtree.
    latest: Option<Instant>,
}

impl SubtreeShared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            gone: watch::channel(false).0,
            below_nanos: AtomicU64::new(0),
            inf_live: watch::channel(0).0,
            below_changed: watch::channel(()).0,
            aborts: Mutex::new(SubtreeAborts::default()),
        })
    }

    fn update_aborts(&self, change: impl FnOnce(&mut SubtreeAborts)) {
        {
            let mut aborts = self.aborts.lock().unwrap_or_else(|p| p.into_inner());
            change(&mut aborts);
        }
        self.below_changed.send_modify(|_| {});
    }

    fn aborts(&self) -> SubtreeAborts {
        *self.aborts.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The record of the first abort issued against one incarnation's task, kept
/// beside its stop lane (every clone of the lane shares it) and outside the
/// lane's state, so it never adds a receiver to the lane.
///
/// Written once: the first abort wins, and a later abort or a re-raised
/// `Kill` never moves it. From the moment an abort is owed (a reaper entry
/// exists for the task, or a ladder raised its `Kill`) until the abort is
/// issued or the actor ends, the record holds an owed mark, counted on the
/// incarnation's subtree token and on every ancestor's. The record holds
/// its token weakly, so it never keeps a subtree alive; the mark holds the
/// shared state of each token it counted on, so its release reaches them
/// whatever ended in between.
#[derive(Clone, Debug, Default)]
pub(crate) struct AbortRecord {
    shared: Arc<AbortShared>,
}

#[derive(Debug)]
struct AbortShared {
    at: watch::Sender<Option<Instant>>,
    token: OnceLock<Weak<SubtreeToken>>,
    owed: Mutex<Owed>,
}

impl Default for AbortShared {
    fn default() -> Self {
        Self {
            at: watch::channel(None).0,
            token: OnceLock::new(),
            owed: Mutex::new(Owed::default()),
        }
    }
}

#[derive(Debug, Default)]
struct Owed {
    mark: Option<OwedMark>,
    /// The actor ended: nothing is owed to it any more.
    ended: bool,
}

/// One owed abort, counted on the shared state of the incarnation's token
/// and every ancestor's; its drop takes the count back.
#[derive(Debug)]
struct OwedMark {
    chain: Vec<Arc<SubtreeShared>>,
}

impl Drop for OwedMark {
    fn drop(&mut self) {
        for shared in &self.chain {
            shared.update_aborts(|a| a.owed = a.owed.saturating_sub(1));
        }
    }
}

impl AbortRecord {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The shared state of the linked token and of every ancestor's.
    fn chain(&self) -> Vec<Arc<SubtreeShared>> {
        let mut chain = Vec::new();
        let token = self.shared.token.get().and_then(Weak::upgrade);
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
        if self.shared.at.borrow().is_some() {
            return;
        }
        let mut owed = self.shared.owed.lock().unwrap_or_else(|p| p.into_inner());
        if owed.ended || owed.mark.is_some() || self.shared.at.borrow().is_some() {
            return;
        }
        let chain = self.chain();
        for shared in &chain {
            shared.update_aborts(|a| a.owed = a.owed.saturating_add(1));
        }
        owed.mark = Some(OwedMark { chain });
    }

    /// An abort is now owed to `task`: marks it as [`AbortRecord::owe`]
    /// does, unless the task already finished, in which case nothing is owed
    /// and the mark is released as [`AbortRecord::abort`] releases it. A
    /// hold that outlives the task (a forwarder still ending) keeps no mark
    /// alive over a task that ended before the owe.
    pub(crate) fn owe_for(&self, task: &AbortHandle) {
        if task.is_finished() {
            self.release();
        } else {
            self.owe();
        }
    }

    /// Aborts `task`, recording the instant unless an abort was recorded
    /// already. Every abort the crate issues against an incarnation's task
    /// goes through here. A task that already finished gets no abort and no
    /// record: nothing is owed to a task that ended, so the owed mark is
    /// released instead, as the actor's end releases it, and a hold that
    /// outlives the task (a forwarder still ending) keeps no mark alive.
    pub(crate) fn abort(&self, task: &AbortHandle) {
        if task.is_finished() {
            self.release();
            return;
        }
        self.record(Instant::now());
        task.abort();
    }

    /// Records `now` as the first abort. A record already written stands.
    fn record(&self, now: Instant) {
        let first = self.shared.at.send_if_modified(|at| {
            if at.is_some() {
                return false;
            }
            *at = Some(now);
            true
        });
        if !first {
            return;
        }
        let mark = self
            .shared
            .owed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .mark
            .take();
        let chain = match &mark {
            Some(mark) => mark.chain.clone(),
            None => self.chain(),
        };
        for shared in &chain {
            shared.update_aborts(|a| a.latest = Some(a.latest.map_or(now, |l| l.max(now))));
        }
        drop(mark);
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
    fn release(&self) {
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
    /// issued. A recorded abort answers the owed one, also in the moment
    /// between its write and the release of the mark.
    pub(crate) fn view(&self) -> (Option<Instant>, bool) {
        let at = *self.shared.at.borrow();
        let owed = at.is_none()
            && self
                .shared
                .owed
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .mark
                .is_some();
        (at, owed)
    }

    /// A receiver woken when the first abort is recorded.
    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<Instant>> {
        self.shared.at.subscribe()
    }

    /// Links the record to the incarnation's subtree token.
    fn link(&self, token: &Arc<SubtreeToken>) {
        let _ = self.shared.token.set(Arc::downgrade(token));
    }
}

/// The token of one spawned actor. Held by the actor's [`SubtreeHold`] and
/// by the tokens of the actors it spawned.
#[derive(Debug)]
pub(crate) struct SubtreeToken {
    shared: Arc<SubtreeShared>,
    /// The time this actor can outlive a killed parent, in nanoseconds.
    own_nanos: u64,
    parent: Option<Arc<SubtreeToken>>,
}

impl SubtreeToken {
    /// Raises the budget below this token to at least `candidate`, and the
    /// parent's the same way.
    fn raise_below(&self, candidate: u64) {
        let previous = self
            .shared
            .below_nanos
            .fetch_max(candidate, Ordering::SeqCst);
        if candidate > previous {
            self.shared.below_changed.send_modify(|_| {});
            if let Some(parent) = &self.parent {
                parent.raise_below(self.own_nanos.saturating_add(candidate));
            }
        }
    }

    /// Raises or lowers by one the live-supervisor count of `token` and of
    /// every ancestor.
    fn shift_infinity(token: &Arc<SubtreeToken>, raise: bool) {
        let mut at = Some(token);
        while let Some(current) = at {
            current.shared.inf_live.send_modify(|count| {
                *count = if raise {
                    count.saturating_add(1)
                } else {
                    count.saturating_sub(1)
                };
            });
            at = current.parent.as_ref();
        }
    }
}

impl Drop for SubtreeToken {
    fn drop(&mut self) {
        self.shared.gone.send_replace(true);
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
}

/// Lowers the live-supervisor count of the ancestors of an unbounded
/// supervisor child when the child's actor ends, whether or not anything
/// below it has.
#[derive(Debug)]
struct InfinityMark {
    above: Arc<SubtreeToken>,
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
    token: Arc<SubtreeToken>,
    _mark: Option<InfinityMark>,
    /// The abort record of the actor's incarnation, released when the hold
    /// drops: the actor ended, so no abort is owed to it any more.
    record: AbortRecord,
}

impl Drop for SubtreeHold {
    fn drop(&mut self) {
        self.record.release();
    }
}

impl SubtreeHold {
    /// The hold of a new actor whose stop lane carries `record`: the record
    /// is linked to the actor's token, so its aborts reach the subtree's
    /// waits.
    pub(crate) fn with_record(link: Option<SubtreeLink>, record: AbortRecord) -> Arc<Self> {
        let hold = Self::build(link, record);
        hold.record.link(&hold.token);
        hold
    }

    /// The hold of a new actor: a root, or the child described by `link`.
    pub(crate) fn new(link: Option<SubtreeLink>) -> Arc<Self> {
        Self::build(link, AbortRecord::new())
    }

    fn build(link: Option<SubtreeLink>, record: AbortRecord) -> Arc<Self> {
        let (token, mark) = match link {
            None => (
                Arc::new(SubtreeToken {
                    shared: SubtreeShared::new(),
                    own_nanos: 0,
                    parent: None,
                }),
                None,
            ),
            Some(link) => {
                let own_nanos = rules::saturating_nanos(link.own);
                let token = Arc::new(SubtreeToken {
                    shared: SubtreeShared::new(),
                    own_nanos,
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
        Arc::new(Self {
            token,
            _mark: mark,
            record,
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
        }
    }
}

/// An observer of one actor's subtree. Cloning it is cheap, and holding it
/// never keeps the subtree alive.
#[derive(Clone, Debug)]
pub(crate) struct SubtreeWatch {
    shared: Arc<SubtreeShared>,
}

impl SubtreeWatch {
    /// Whether the actor and everything below it ended.
    pub(crate) fn is_gone(&self) -> bool {
        *self.shared.gone.borrow()
    }

    /// The live-supervisor count below the actor.
    fn infinity_live(&self) -> u32 {
        *self.shared.inf_live.borrow()
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
        flag_set(&mut self.shared.gone.subscribe()).await;
    }

    /// Resolves once no supervisor child with an unbounded orderly stop is
    /// alive below the actor, at once when none is.
    pub(crate) async fn infinity_cleared(&self) {
        let mut live = self.shared.inf_live.subscribe();
        let _ = live.wait_for(|live| *live == 0).await;
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
    watch: SubtreeWatch,
    gone: watch::Receiver<bool>,
    infinity: watch::Receiver<u32>,
    below: watch::Receiver<()>,
    deadline: Option<Instant>,
    /// The bound the deadline was last armed with: the cap of a wait for an
    /// owed abort counts it once more from the deadline.
    bound: Duration,
    /// The budget read when the deadline was last armed: a wake on the
    /// budget's signal that finds it unchanged came from the subtree's
    /// aborts and arms nothing.
    below_seen: u64,
}

/// What woke a [`SubtreeWait`].
enum WaitWake {
    Gone,
    Infinity,
    Below,
    Deadline,
}

impl SubtreeWait {
    /// Starts a wait on `watch`, arming its deadline when it has a bound.
    pub(crate) fn new(watch: &SubtreeWatch) -> Self {
        let mut wait = Self {
            watch: watch.clone(),
            gone: watch.shared.gone.subscribe(),
            infinity: watch.shared.inf_live.subscribe(),
            below: watch.shared.below_changed.subscribe(),
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
    fn verdict_at(&self) -> Option<Instant> {
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
    fn rearm(&mut self) {
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
    fn rearm_if_raised(&mut self) {
        if self.watch.shared.below_nanos.load(Ordering::SeqCst) != self.below_seen {
            self.rearm();
        }
    }

    /// Resolves at the next change that can decide the wait: the subtree
    /// ending, the count of unbounded supervisor children or the budget
    /// changing (after which the deadline is re-read), a change of the
    /// subtree's aborts, or the instant the verdict may be given.
    pub(crate) async fn changed(&mut self) {
        let wake = {
            let deadline = self.verdict_at();
            tokio::select! {
                biased;
                _ = flag_set(&mut self.gone) => WaitWake::Gone,
                _ = self.infinity.changed() => WaitWake::Infinity,
                _ = self.below.changed() => WaitWake::Below,
                _ = sleep_until_opt(deadline) => WaitWake::Deadline,
            }
        };
        match wake {
            WaitWake::Infinity => self.rearm(),
            WaitWake::Below => self.rearm_if_raised(),
            WaitWake::Gone | WaitWake::Deadline => {}
        }
    }
}

/// Resolves once the flag reads set.
async fn flag_set(flag: &mut watch::Receiver<bool>) {
    let _ = flag.wait_for(|set| *set).await;
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => sleep_until(at).await,
        None => std::future::pending::<()>().await,
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
    fn child_of(root: &Arc<SubtreeHold>) -> (Arc<SubtreeHold>, AbortRecord) {
        let record = AbortRecord::new();
        let link = SubtreeLink {
            parent: root.token(),
            own: ms(200),
            infinity: false,
        };
        (SubtreeHold::with_record(Some(link), record.clone()), record)
    }

    #[tokio::test(start_paused = true)]
    async fn an_owed_abort_is_counted_up_the_chain_and_released_by_its_record() {
        let root = SubtreeHold::new(None);
        let (child, record) = child_of(&root);
        let task = tokio::spawn(std::future::pending::<()>());
        record.owe();
        record.owe();
        assert_eq!(root.watch().abort_view(), (None, true));
        assert_eq!(child.watch().abort_view(), (None, true));
        tokio::time::advance(ms(30)).await;
        let at = Instant::now();
        record.abort(&task.abort_handle());
        assert_eq!(root.watch().abort_view(), (Some(at), false));
        assert_eq!(child.watch().abort_view(), (Some(at), false));
        record.owe();
        assert_eq!(
            record.view(),
            (Some(at), false),
            "nothing is owed after an abort"
        );
    }

    /// The end of an actor that was owed an abort releases the mark and
    /// counts as the subtree's latest abort, so a wait on the subtree gives
    /// the cascade of the end the settle time.
    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_actor_releases_its_owed_abort_and_none_is_taken_again() {
        let root = SubtreeHold::new(None);
        let (child, record) = child_of(&root);
        record.owe();
        tokio::time::advance(ms(30)).await;
        let end = Instant::now();
        drop(child);
        assert_eq!(root.watch().abort_view(), (Some(end), false));
        record.owe();
        assert_eq!(
            root.watch().abort_view(),
            (Some(end), false),
            "an ended actor owes nothing"
        );
        assert_eq!(record.view(), (None, false));
    }

    /// The end of an actor that was owed nothing raises nothing: a subtree
    /// with nothing owed and nothing recorded keeps its deadline exactly.
    #[tokio::test(start_paused = true)]
    async fn the_end_of_an_actor_owed_nothing_raises_nothing() {
        let root = SubtreeHold::new(None);
        let (child, record) = child_of(&root);
        assert_eq!(root.watch().abort_view(), (None, false));
        drop(child);
        assert_eq!(root.watch().abort_view(), (None, false));
        assert_eq!(record.view(), (None, false));
        record.owe();
        assert_eq!(
            root.watch().abort_view(),
            (None, false),
            "an ended actor owes nothing"
        );
    }

    /// A reader of the subtree never sees it with nothing owed and nothing
    /// recorded while an owed actor ends: the end is recorded before the
    /// mark drops. The reader runs on its own thread, so it reads at every
    /// point of the release.
    #[test]
    fn a_reader_never_sees_the_end_of_an_owed_actor_as_neither_owed_nor_recorded() {
        for _ in 0..2_000 {
            let root = SubtreeHold::new(None);
            let (child, record) = child_of(&root);
            record.owe();
            let watch = root.watch();
            let stop = Arc::new(AtomicU64::new(0));
            let started = Arc::new(AtomicU64::new(0));
            let reader = {
                let (stop, started) = (stop.clone(), started.clone());
                std::thread::spawn(move || {
                    let mut gap = false;
                    while stop.load(Ordering::SeqCst) == 0 {
                        let (latest, owed) = watch.abort_view();
                        started.store(1, Ordering::SeqCst);
                        gap |= latest.is_none() && !owed;
                    }
                    gap
                })
            };
            while started.load(Ordering::SeqCst) == 0 {
                std::hint::spin_loop();
            }
            drop(child);
            stop.store(1, Ordering::SeqCst);
            assert!(
                !reader.join().unwrap(),
                "the subtree read as neither owed nor recorded during the release"
            );
            assert!(!root.watch().abort_view().1, "the mark was released");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_abort_stays_recorded_on_the_record_and_up_the_chain() {
        let root = SubtreeHold::new(None);
        let (_child, record) = child_of(&root);
        let first = Instant::now();
        record.record(first);
        record.record(first + ms(100));
        assert_eq!(record.view(), (Some(first), false));
        assert_eq!(root.watch().abort_view(), (Some(first), false));
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_task_records_nothing() {
        let record = AbortRecord::new();
        let task = tokio::spawn(async {});
        tokio::task::yield_now().await;
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        record.abort(&task.abort_handle());
        assert_eq!(record.view(), (None, false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::runtime::FAR_FUTURE;
    use crate::actor::supervision::ABORT_SETTLE;

    fn child(parent: &Arc<SubtreeHold>, own: Duration, infinity: bool) -> Arc<SubtreeHold> {
        SubtreeHold::new(Some(SubtreeLink {
            parent: parent.token(),
            own,
            infinity,
        }))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn gone_fires_only_after_every_descendant_dropped() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let kid = child(&root, ms(10), false);
        let grandkid = child(&kid, ms(10), false);
        drop(root);
        assert!(!watch.is_gone(), "descendants still hold the token");
        drop(kid);
        assert!(!watch.is_gone(), "the grandchild still holds it");
        drop(grandkid);
        assert!(watch.is_gone());
    }

    #[test]
    fn below_is_the_max_of_children_budgets_and_never_decreases() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        assert_eq!(watch.release_bound(), Some(ABORT_SETTLE));
        let small = child(&root, ms(100), false);
        assert_eq!(watch.release_bound(), Some(ms(100) + ABORT_SETTLE));
        let large = child(&root, ms(500), false);
        assert_eq!(watch.release_bound(), Some(ms(500) + ABORT_SETTLE));
        let _smaller = child(&root, ms(200), false);
        assert_eq!(watch.release_bound(), Some(ms(500) + ABORT_SETTLE));
        // A grandchild's budget adds to its parent's own and reaches the root.
        let _deep = child(&small, ms(900), false);
        assert_eq!(watch.release_bound(), Some(ms(100 + 900) + ABORT_SETTLE));
        // A child that ends never lowers the bound.
        drop(large);
        assert_eq!(watch.release_bound(), Some(ms(100 + 900) + ABORT_SETTLE));
    }

    #[test]
    fn an_unbounded_child_makes_the_release_bound_unbounded() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let _kid = child(&root, Duration::MAX, false);
        assert_eq!(watch.release_bound(), None);
    }

    #[test]
    fn huge_budgets_saturate_to_unbounded_instead_of_wrapping() {
        // Two levels whose sum overflows a u64 of nanoseconds: unbounded,
        // never a small wrapped value.
        let half = Duration::from_nanos(u64::MAX / 2 + 1);
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let kid = child(&root, half, false);
        let _grandkid = child(&kid, half, false);
        assert_eq!(watch.release_bound(), None);

        // A duration just beyond a u64 of nanoseconds reads as unbounded, not
        // as the few nanoseconds a truncating cast would leave of it.
        let beyond = Duration::new(18_446_744_073, 709_551_621);
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let _kid = child(&root, beyond, false);
        assert_eq!(watch.release_bound(), None);

        // A budget of 30 years fits a u64 of nanoseconds: a finite bound, the
        // cap every wait carries.
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let _kid = child(&root, FAR_FUTURE, false);
        assert_eq!(watch.release_bound(), Some(FAR_FUTURE));
    }

    #[tokio::test]
    async fn gone_fires_from_an_aborted_task_s_drop_glue() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let held = root.clone();
        drop(root);
        let task = tokio::spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert!(!watch.is_gone());
        task.abort();
        let _ = task.await;
        tokio::time::timeout(ms(1000), watch.gone())
            .await
            .expect("the aborted task's drop glue fires gone");
    }

    #[tokio::test]
    async fn release_bound_is_unbounded_only_while_an_infinity_supervisor_child_lives_below() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let first = child(&root, ms(50), true);
        let second = child(&root, ms(50), true);
        assert_eq!(watch.release_bound(), None);
        drop(first);
        assert_eq!(watch.release_bound(), None, "one still lives");
        // An aborted child's drop glue lowers the count too.
        let held = second.clone();
        drop(second);
        let task = tokio::spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(watch.release_bound(), None);
        task.abort();
        let _ = task.await;
        tokio::time::timeout(ms(1000), watch.infinity_cleared())
            .await
            .expect("the count reaches zero");
        assert_eq!(watch.release_bound(), Some(ms(50) + ABORT_SETTLE));
    }

    #[tokio::test]
    async fn an_ended_infinity_supervisor_child_with_a_live_descendant_no_longer_counts() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let supervisor = child(&root, ms(50), true);
        let worker = child(&supervisor, ms(50), false);
        assert_eq!(watch.release_bound(), None);
        // The supervisor child's own actor ends while its worker still runs:
        // nothing unbounded is alive any more, though the subtree is not gone.
        drop(supervisor);
        assert_eq!(watch.release_bound(), Some(ms(100) + ABORT_SETTLE));
        assert!(!watch.is_gone());
        drop(worker);
        drop(root);
        assert!(watch.is_gone());
    }

    #[tokio::test(start_paused = true)]
    async fn release_bound_counts_from_the_end_of_the_last_infinity_supervisor_child() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let _worker = child(&root, ms(200), false);
        let first = child(&root, ms(50), true);
        let second = child(&root, ms(50), true);
        let mut wait = SubtreeWait::new(&watch);
        assert_eq!(wait.deadline(), None, "unbounded while they live");
        tokio::time::advance(ms(10_000)).await;
        assert!(!wait.expired());
        drop(first);
        wait.changed().await;
        assert_eq!(wait.deadline(), None, "one still lives");
        let ended_at = Instant::now();
        drop(second);
        wait.changed().await;
        assert_eq!(
            wait.deadline(),
            Some(ended_at + ms(200) + ABORT_SETTLE),
            "counted from the moment the count reached zero"
        );
        assert!(!wait.expired());
        tokio::time::advance(ms(200) + ABORT_SETTLE).await;
        assert!(wait.expired());
    }

    #[tokio::test(start_paused = true)]
    async fn release_bound_returns_to_unbounded_when_an_infinity_supervisor_child_appears_after_arming(
    ) {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let mut wait = SubtreeWait::new(&watch);
        assert!(
            wait.deadline().is_some(),
            "armed at once: nothing unbounded lives"
        );
        let kid = child(&root, ms(50), true);
        wait.changed().await;
        assert_eq!(wait.deadline(), None, "disarmed again");
        tokio::time::advance(ms(10_000)).await;
        assert!(!wait.expired());
        drop(kid);
        wait.changed().await;
        assert!(wait.deadline().is_some(), "armed again once it ended");
    }

    #[tokio::test(start_paused = true)]
    async fn a_below_raise_after_arming_extends_the_deadline() {
        let root = SubtreeHold::new(None);
        let watch = root.watch();
        let mut wait = SubtreeWait::new(&watch);
        let armed = wait.deadline().expect("armed");
        tokio::time::advance(ms(40)).await;
        let _kid = child(&root, ms(500), false);
        wait.changed().await;
        let extended = wait.deadline().expect("armed");
        assert!(extended > armed);
        assert_eq!(extended, Instant::now() + ms(500) + ABORT_SETTLE);
        // A raise that stays below the armed deadline never shortens it.
        let before = wait.deadline();
        let _small = child(&root, ms(10), false);
        assert_eq!(wait.deadline(), before);
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_subtree_reads_as_ended_only_after_the_actor_value_is_dropped() {
        // Read from this thread, which runs no task of the runtime: the
        // answer cannot wait behind the dropping worker. Both ways an actor
        // ends are covered: it returns, or its task is aborted.
        for abort in [false, true] {
            let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (handle, join, watch) = crate::actor::runtime::spawn_actor(
                crate::types::ActorId::from(format!("slow-drop-{abort}")),
                Slow {
                    dropped: dropped.clone(),
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
            if abort {
                join.abort();
            } else {
                handle
                    .stop(crate::types::StopReason::Graceful)
                    .await
                    .expect("the stop is accepted");
            }
            let started = std::time::Instant::now();
            while !watch.is_gone() {
                assert!(started.elapsed() < Duration::from_secs(5), "never ended");
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(
                dropped.load(Ordering::SeqCst),
                "abort {abort}: the subtree read as ended before the actor value was dropped"
            );
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

    /// A task dropped before its first poll drops its parameters in
    /// declaration order, so the hold of the registry guard is the one that
    /// keeps the subtree open past the mailbox: the name is free when the
    /// subtree reads as ended.
    #[tokio::test]
    async fn an_actor_aborted_before_its_first_poll_frees_its_name_before_its_subtree_ends() {
        let system =
            crate::system::ActorSystem::create(format!("unpolled-{}", uuid::Uuid::new_v4()))
                .unwrap();
        let name = format!("unpolled-{}", uuid::Uuid::new_v4());
        let (handle, join, watch) = crate::actor::runtime::spawn_actor(
            crate::types::ActorId::from(name.clone()),
            Named,
            crate::actor::runtime::ActorConfig::default(),
            Some(name.clone()),
            Some(system.clone()),
            None,
            true,
            Duration::from_millis(120),
            None,
        )
        .expect("the actor spawns");
        // The runtime has not polled the task: nothing awaited since the spawn.
        handle.try_notify(SlowMsg).expect("the mailbox has room");
        join.abort();
        let observer = read_at_the_end(watch, {
            let name = name.clone();
            move || system.get::<Named>(&name).is_none()
        });
        let _ = join.await;
        assert!(
            observer.join().unwrap(),
            "the name was still registered when the subtree read as ended"
        );
    }

    /// A task dropped while suspended drops its parameters last to first, so
    /// the hold declared first is released after the actor value.
    #[tokio::test]
    async fn an_actor_aborted_while_it_starts_is_dropped_before_its_subtree_ends() {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (handle, join, watch) = crate::actor::runtime::spawn_actor(
            crate::types::ActorId::from(format!("parked-{}", uuid::Uuid::new_v4())),
            Parked {
                dropped: dropped.clone(),
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
        let status = handle.status_rx();
        // Let the task run to its parking point in `on_started`.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        join.abort();
        let observer = read_at_the_end(watch, {
            let dropped = dropped.clone();
            move || dropped.load(Ordering::SeqCst)
        });
        // The status plane reads as ended, its sender gone, only once the
        // actor value is dropped.
        let status_observer = {
            let dropped = dropped.clone();
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                while status.has_changed().is_ok() {
                    assert!(started.elapsed() < Duration::from_secs(5), "never ended");
                    std::thread::sleep(Duration::from_millis(1));
                }
                dropped.load(Ordering::SeqCst)
            })
        };
        let _ = join.await;
        assert!(
            observer.join().unwrap(),
            "the subtree read as ended before the actor value was dropped"
        );
        assert!(
            status_observer.join().unwrap(),
            "the status plane read as ended before the actor value was dropped"
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopped_means_the_actor_value_is_dropped_multi_thread() {
        stopped_means_the_actor_value_is_dropped().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stopped_means_the_actor_value_is_dropped_current_thread() {
        stopped_means_the_actor_value_is_dropped().await;
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_actor_drop_ends_in_one_reason_multi_thread() {
        a_panicking_actor_drop_ends_in_one_reason().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_panicking_actor_drop_ends_in_one_reason_current_thread() {
        a_panicking_actor_drop_ends_in_one_reason().await;
    }
}

/// A wait on a subtree gives its verdict no later than its deadline plus its
/// bound plus the settle time, whatever its members do near that cap: an
/// end or an abort recorded within a settle time of the last one raises the
/// floor no further than the ceiling.
#[cfg(test)]
mod ceiling_tests {
    use super::*;
    use crate::actor::supervision::ABORT_SETTLE;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A child below `parent` with `own` budget, its record linked to its
    /// token.
    fn below(parent: &Arc<SubtreeHold>, own: Duration) -> (Arc<SubtreeHold>, AbortRecord) {
        let record = AbortRecord::new();
        let hold = SubtreeHold::with_record(
            Some(SubtreeLink {
                parent: parent.token(),
                own,
                infinity: false,
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
        p_rec: &AbortRecord,
    ) -> (SubtreeWait, Instant, Duration) {
        let wait = SubtreeWait::new(&p.watch());
        let start = Instant::now();
        let deadline = wait.deadline().unwrap();
        let bound = deadline - start;
        p_rec.owe();
        tokio::time::advance(bound + bound - ms(1)).await;
        (wait, deadline, bound)
    }

    /// An owed supervisor P obeys its late Kill and ends 1 ms before its
    /// wait's cap. Its worker child Z is handed to its guard (owed, with a
    /// kill grace of 50 ms, below the settle time), never yields, and is
    /// aborted 50 ms after P's end; Z lives on. The verdict lands no later
    /// than the ceiling.
    #[tokio::test(start_paused = true)]
    async fn a_short_grace_abort_below_a_supervisor_that_ended_at_the_cap_keeps_the_ceiling() {
        let p_rec = AbortRecord::new();
        let p = SubtreeHold::with_record(None, p_rec.clone());
        let (z, z_rec) = below(&p, ms(300));
        let (wait, deadline, bound) = owed_root_at_the_cap(&p, &p_rec).await;
        let from = Instant::now();
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        let mut p = Some(p);
        let at = first_verdict(&wait, |t| {
            if t == 0 {
                drop(p.take());
                z_rec.owe();
            }
            if t == 50 {
                z_rec.abort(&handle);
            }
        })
        .await;
        let at = from + ms(at);
        let ceiling = deadline + bound + ABORT_SETTLE;
        drop(z);
        assert!(at <= ceiling, "verdict {:?} past the ceiling", at - ceiling);
        assert!(
            at >= deadline + bound,
            "verdict {:?} before the cap",
            deadline + bound - at
        );
    }

    /// With the default kill grace: P ends 1 ms before the cap; its
    /// supervisor child A (owed by its guard) obeys its Kill 90 ms later and
    /// ends; its worker child Z (owed, kill grace 120 ms) never yields and is
    /// aborted 120 ms after P's end. Each end and abort within a settle time
    /// of the last raises the floor, and the ceiling still holds.
    #[tokio::test(start_paused = true)]
    async fn owed_ends_below_a_supervisor_that_ended_at_the_cap_keep_the_ceiling() {
        let p_rec = AbortRecord::new();
        let p = SubtreeHold::with_record(None, p_rec.clone());
        let (a, a_rec) = below(&p, ms(300));
        let (z, z_rec) = below(&p, ms(300));
        let (wait, deadline, bound) = owed_root_at_the_cap(&p, &p_rec).await;
        let from = Instant::now();
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        let (mut p, mut a) = (Some(p), Some(a));
        let at = first_verdict(&wait, |t| {
            if t == 0 {
                drop(p.take());
                a_rec.owe();
                z_rec.owe();
            }
            if t == 90 {
                drop(a.take());
            }
            if t == 120 {
                z_rec.abort(&handle);
            }
        })
        .await;
        let at = from + ms(at);
        let ceiling = deadline + bound + ABORT_SETTLE;
        drop(z);
        assert!(at <= ceiling, "verdict {:?} past the ceiling", at - ceiling);
        assert!(
            at >= deadline + bound,
            "verdict {:?} before the cap",
            deadline + bound - at
        );
    }

    /// P is aborted (recorded) 1 ms before the cap and ends at the abort;
    /// its worker child Z, with a 50 ms kill grace, is aborted 50 ms later
    /// and lives on. The verdict lands no later than the ceiling.
    #[tokio::test(start_paused = true)]
    async fn a_short_grace_abort_below_a_supervisor_aborted_at_the_cap_keeps_the_ceiling() {
        let p_rec = AbortRecord::new();
        let p = SubtreeHold::with_record(None, p_rec.clone());
        let (z, z_rec) = below(&p, ms(300));
        let (wait, deadline, bound) = owed_root_at_the_cap(&p, &p_rec).await;
        let from = Instant::now();
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        let p_task = tokio::spawn(std::future::pending::<()>());
        let p_handle = p_task.abort_handle();
        let mut p = Some(p);
        let at = first_verdict(&wait, |t| {
            if t == 0 {
                p_rec.abort(&p_handle);
                drop(p.take());
                z_rec.owe();
            }
            if t == 50 {
                z_rec.abort(&handle);
            }
        })
        .await;
        let at = from + ms(at);
        let ceiling = deadline + bound + ABORT_SETTLE;
        drop(z);
        assert!(at <= ceiling, "verdict {:?} past the ceiling", at - ceiling);
        assert!(
            at >= deadline + bound,
            "verdict {:?} before the cap",
            deadline + bound - at
        );
    }
}
