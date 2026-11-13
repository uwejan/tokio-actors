//! The state both ends of a link share, the signal inbox and the pair table of
//! a shared block, and the stage that delivers the exit signals an ending
//! actor still owes, on bare blocks and on small actors.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Wake, Waker};
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::time::timeout;

use super::link::{ExitPush, InboxItem, LinkState, PushResult};
use super::subtree::AbortRecord;
use crate::actor::context::ActorContext;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult};
use crate::types::{ActorId, ActorStatus, LinkExit, PeerRef, StopLane, StopReason};

const BOUND: Duration = Duration::from_secs(10);

/// A block of its own, with the lane that keeps it.
fn block() -> (StopLane, AbortRecord) {
    let (lane, rx) = StopLane::new();
    std::mem::forget(rx);
    let record = lane.abort_record().clone();
    (lane, record)
}

fn peer_of(record: &AbortRecord, name: &str) -> PeerRef {
    PeerRef {
        actor_id: ActorId::from(name),
        incarnation: record.incarnation(),
    }
}

/// A link item from `from`, whose plane is `from`'s own.
fn link_item(from: &AbortRecord, state: &Arc<LinkState>) -> InboxItem {
    let (_tx, plane) = from.status_channel();
    InboxItem::Link {
        state: state.clone(),
        from: peer_of(from, "from"),
        plane,
    }
}

/// A pair is removed only for the state it holds: the end of an earlier link
/// between the same two runs never removes a later link's pair.
#[test]
fn remove_pair_matches_only_its_state() {
    let (_la, a) = block();
    let (_lb, b) = block();
    let (low, high) = if a.incarnation() < b.incarnation() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    let (old, _) = low.pair_state(low.incarnation(), high);
    low.remove_pair(low.incarnation(), high.incarnation(), &old);
    let (newer, made) = low.pair_state(low.incarnation(), high);
    assert!(made && !Arc::ptr_eq(&old, &newer));
    low.remove_pair(low.incarnation(), high.incarnation(), &old);
    assert_eq!(low.pairs_for_test(), 1, "the later link's pair stays");
    low.remove_pair(low.incarnation(), high.incarnation(), &newer);
    assert_eq!(low.pairs_for_test(), 0);
}

/// A push into a sealed inbox is refused and queues nothing, an exit signal
/// reads the flag under the lock, a sealed block hands out a state of its
/// own, and a trapping block bounds the exit signals waiting for it.
#[test]
fn push_after_seal_returns_sealed() {
    let (_lane, record) = block();
    let (_lo, other) = block();
    let state = Arc::new(LinkState::new());
    assert_eq!(
        record.push_link(link_item(&other, &state)),
        PushResult::Pushed
    );
    assert!(record.signals_pending());
    assert_eq!(
        record.push_exit_or_trap(peer_of(&other, "o"), StopReason::Graceful),
        ExitPush::NotTrapping,
        "a target that does not trap queues nothing"
    );
    record.set_trap(true);
    assert_eq!(
        record.push_exit_or_trap(peer_of(&other, "o"), StopReason::Graceful),
        ExitPush::Pushed
    );
    let taken = record.take_and_seal_signals();
    assert_eq!(taken.len(), 2);
    assert!(!record.signals_pending());
    assert_eq!(
        record.push_link(link_item(&other, &state)),
        PushResult::Sealed
    );
    assert_eq!(
        record.push_unlink(other.incarnation(), state.clone()),
        PushResult::Sealed
    );
    assert_eq!(
        record.push_exit_or_trap(peer_of(&other, "o"), StopReason::Graceful),
        ExitPush::Sealed
    );
    assert!(!record.signals_pending());
    assert!(record.take_signals().is_empty());
    let (a, made_a) = record.pair_state(record.incarnation(), &other);
    let (b, made_b) = record.pair_state(record.incarnation(), &other);
    assert!(made_a && made_b && !Arc::ptr_eq(&a, &b));

    // A trapping block takes as many exit signals as its bound allows and
    // refuses the next one; a drain alone gives nothing back, and each count
    // given back lets one more in.
    let (_lb, bounded) = block();
    bounded.set_exit_cap(2);
    bounded.set_trap(true);
    let exit = || bounded.push_exit_or_trap(peer_of(&other, "o"), StopReason::Graceful);
    assert_eq!(exit(), ExitPush::Pushed);
    assert_eq!(exit(), ExitPush::Pushed);
    assert_eq!(exit(), ExitPush::Full, "the bound is reached");
    assert_eq!(
        bounded.take_signals().len(),
        2,
        "a refused signal queues nothing"
    );
    assert_eq!(exit(), ExitPush::Full, "a drain alone gives nothing back");
    bounded.release_exit();
    assert_eq!(exit(), ExitPush::Pushed, "a count given back lets one in");
}

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: AtomicUsize,
}

impl Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a waker's wake panics");
        }
    }
}

/// The wake that follows a push belongs to the run loop's waker slot, and a
/// panic of that waker stays out of the push: the item is queued, the caller
/// goes on, and the inbox reads pending.
#[test]
fn signal_inbox_wake_is_contained() {
    for exit in [false, true] {
        let (_lane, record) = block();
        let (_lo, other) = block();
        record.set_trap(true);
        let loud = Arc::new(LoudWake::default());
        let waker = Waker::from(loud.clone());
        record.reader().register(&waker);
        loud.armed.store(true, Ordering::SeqCst);
        let pushed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if exit {
                record.push_exit_or_trap(peer_of(&other, "o"), StopReason::Kill) == ExitPush::Pushed
            } else {
                record.push_link(link_item(&other, &Arc::new(LinkState::new())))
                    == PushResult::Pushed
            }
        }));
        loud.armed.store(false, Ordering::SeqCst);
        assert_eq!(
            pushed.ok(),
            Some(true),
            "exit {exit}: a waker's panic unwound into the push"
        );
        assert_eq!(loud.woken.load(Ordering::SeqCst), 1, "exit {exit}");
        assert!(record.signals_pending());
        assert_eq!(record.take_signals().len(), 1);
    }
}

// ---------------------------------------------------------------------------
// The stage, on small actors
// ---------------------------------------------------------------------------

enum Msg {
    Trap,
    /// Signals `entered`, then waits for `release`.
    Hold(Arc<Semaphore>, Arc<Semaphore>),
    Exit(LinkExit),
}

#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Exit(LinkExit, ActorStatus),
    Stopped(StopReason),
}

struct Probe(mpsc::UnboundedSender<Seen>);

impl Actor for Probe {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Trap => ctx.trap_exit(Msg::Exit),
            Msg::Hold(entered, release) => {
                entered.add_permits(1);
                let _ = release.acquire().await;
            }
            Msg::Exit(exit) => {
                let _ = self.0.send(Seen::Exit(exit, ctx.status()));
            }
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.0.send(Seen::Stopped(reason.clone()));
        Ok(())
    }
}

fn failure() -> StopReason {
    StopReason::Failure(ActorError::user("peer failed"))
}

/// A trapping probe held inside a turn, and what it logs.
async fn held_probe() -> (
    crate::actor::handle::ActorHandle<Probe>,
    mpsc::UnboundedReceiver<Seen>,
    Arc<Semaphore>,
) {
    let (log, seen) = mpsc::unbounded_channel();
    let probe = Probe(log).spawn().await.unwrap();
    probe.send(Msg::Trap).await.unwrap();
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    probe
        .notify(Msg::Hold(entered.clone(), release.clone()))
        .await
        .unwrap();
    timeout(BOUND, entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    (probe, seen, release)
}
