//! A callback is timed only if it began after a threshold was set: one already
//! in flight when the threshold is first set reports no elapsed time and is
//! never listed as stuck, and the next callback that actor starts is timed.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorSystem,
};

const HANG_BOUND: Duration = Duration::from_secs(60);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "turn-timing-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + HANG_BOUND;
    while !pred() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// A turn the test holds: the handler records that it began, then blocks its
/// thread until the test opens the turn (at most `HANG_BOUND`).
#[derive(Clone, Default)]
struct HeldTurn(Arc<HeldTurnState>);

#[derive(Default)]
struct HeldTurnState {
    began: AtomicBool,
    open: AtomicBool,
    left: AtomicBool,
}

impl HeldTurn {
    fn block(&self) {
        self.0.began.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + HANG_BOUND;
        while !self.0.open.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.0.left.store(true, Ordering::SeqCst);
    }

    async fn began(&self) {
        until("the held turn to begin", || {
            self.0.began.load(Ordering::SeqCst)
        })
        .await;
    }

    async fn left(&self) {
        until("the held turn to leave its block", || {
            self.0.left.load(Ordering::SeqCst)
        })
        .await;
    }

    fn open(&self) {
        self.0.open.store(true, Ordering::SeqCst);
    }

    /// Holds the next turn again. Called only between two turns.
    fn rearm(&self) {
        self.0.began.store(false, Ordering::SeqCst);
        self.0.left.store(false, Ordering::SeqCst);
        self.0.open.store(false, Ordering::SeqCst);
    }
}

/// Opens the turn when dropped, so a failed assertion never leaves a thread
/// blocked.
struct OpenOnDrop(HeldTurn);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

struct SlowHandler(HeldTurn);

impl Actor for SlowHandler {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.0.block();
        Ok(())
    }
}

async fn slow_on(sys: &Arc<ActorSystem>) -> (ActorHandle<SlowHandler>, HeldTurn, OpenOnDrop) {
    let turn = HeldTurn::default();
    let handle = SlowHandler(turn.clone())
        .spawn()
        .on_system(sys)
        .await
        .unwrap();
    let guard = OpenOnDrop(turn.clone());
    (handle, turn, guard)
}

#[test]
fn a_callback_in_flight_when_the_threshold_is_set_is_not_timed() {
    common::run_multi_thread_only(3, async {
        let sys = ActorSystem::create(uname("set-mid-turn")).unwrap();
        assert_eq!(sys.turn_warning_threshold(), None);

        let (before, turn_before, _open_before) = slow_on(&sys).await;
        before.notify(()).await.unwrap();
        turn_before.began().await; // began with no threshold set

        sys.set_turn_warning_threshold(Some(Duration::from_millis(1)));
        assert_eq!(
            sys.turn_elapsed(before.id()),
            None,
            "a callback that began before the threshold was set is not timed"
        );
        assert!(
            sys.stuck_actors().iter().all(|(id, _)| id != before.id()),
            "and it is not listed as stuck"
        );
        turn_before.open();

        // A second actor's callback, begun with the threshold set, is timed.
        let (after, turn_after, _open_after) = slow_on(&sys).await;
        after.notify(()).await.unwrap();
        turn_after.began().await;
        assert!(
            sys.turn_elapsed(after.id()).is_some(),
            "a callback that began with the threshold set is timed"
        );
        until("the timed callback to be listed as stuck", || {
            sys.stuck_actors().iter().any(|(id, _)| id == after.id())
        })
        .await;
        turn_after.open();

        // The first actor's next callback is timed too: the second message is
        // handled only after the first callback ended.
        turn_before.left().await;
        turn_before.rearm();
        before.notify(()).await.unwrap();
        turn_before.began().await;
        assert!(
            sys.turn_elapsed(before.id()).is_some(),
            "the next callback of the actor that was mid-turn is timed"
        );
        turn_before.open();
    });
}
