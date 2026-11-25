use super::*;

/// A teardown's waits are polled only after their own wakers fired: 64
/// waits that settle one at a time, the last first, cost one poll each
/// to register and one to settle, where polling every pending wait at
/// every wake costs 2,080.
#[test]
fn a_teardown_wait_polls_only_the_waits_that_were_woken() {
    const WAITS: usize = 64;
    let mut senders = Vec::with_capacity(WAITS);
    let mut waits: Vec<CellWait> = Vec::with_capacity(WAITS);
    for _ in 0..WAITS {
        let (tx, mut rx) = oneshot::channel::<()>();
        senders.push(Some(tx));
        waits.push(Box::pin(std::future::poll_fn(move |cx| {
            Pin::new(&mut rx).poll(cx).map(|_| CellOutcome::Cancelled)
        })));
    }
    let mut set = SettleSet::new(waits);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    {
        let mut first = std::pin::pin!(set.next_settled());
        assert!(first.as_mut().poll(&mut cx).is_pending());
    }
    for index in (0..WAITS).rev() {
        let _ = senders[index].take().unwrap().send(());
        let mut next = std::pin::pin!(set.next_settled());
        match next.as_mut().poll(&mut cx) {
            Poll::Ready((settled, CellOutcome::Cancelled)) => assert_eq!(settled, index),
            other => panic!("wait {index} did not settle: {:?}", other.is_ready()),
        }
    }
    assert_eq!(set.pending(), 0);
    assert!(set.polls <= 2 * WAITS, "{} polls", set.polls);
}

/// An answer nobody receives is dropped inside a catch: a panic of its
/// drop comes back as a payload, at a veto and at the seal alike.
#[test]
fn an_undelivered_answer_whose_drop_panics_is_reported() {
    struct Bomb;
    impl Drop for Bomb {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                panic!("answer drop");
            }
        }
    }
    let mut held = HeldAnswer::<Bomb>::new();
    let (tx, rx) = oneshot::channel();
    drop(rx);
    held.hold(tx, Ok(Bomb), &held_owner(), &IncarnationRef::new());
    assert_eq!(held.release_now().as_deref(), Some("answer drop"));

    let (tx, rx) = oneshot::channel();
    drop(rx);
    held.hold(tx, Ok(Bomb), &held_owner(), &IncarnationRef::new());
    assert_eq!(
        held.deliver(&StopReason::Graceful).as_deref(),
        Some("answer drop")
    );

    // A task that ends before the seal drops the held answer inside the
    // catch: the panic of its drop does not leave the drop.
    let (tx, rx) = oneshot::channel();
    drop(rx);
    held.hold(tx, Ok(Bomb), &held_owner(), &IncarnationRef::new());
    let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(held)));
    assert!(ended.is_ok(), "the held value's drop panic was contained");
}

/// The id a held answer of these tests is logged under.
fn held_owner() -> ActorId {
    ActorId::from("held")
}

#[tokio::test]
async fn saturating_deadline_always_leaves_timer_headroom() {
    let base = Instant::now();

    // A deadline that leaves no room for the timer's own rounding-up
    // addition is unsafe regardless of how it was produced: assert the
    // margin directly instead of trusting any particular code path.
    let assert_has_headroom = |dur: Duration| {
        let deadline = saturating_deadline(base, dur);
        assert!(
            deadline.checked_add(Duration::from_secs(1)).is_some(),
            "deadline for {dur:?} left less than a second of timer headroom"
        );
    };

    assert_has_headroom(Duration::MAX);
    assert_has_headroom(Duration::from_secs(u64::MAX));

    // The largest duration `base.checked_add` still accepts is the exact
    // boundary a plain `checked_add`-based implementation would return
    // unchanged: find it by binary search down to 1ns precision, so this
    // probes the edge directly instead of only far-overflowing inputs.
    let mut lo = Duration::ZERO;
    let mut hi = Duration::from_secs(u64::MAX / 2);
    while hi - lo > Duration::from_nanos(1) {
        let mid = lo + (hi - lo) / 2;
        if base.checked_add(mid).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    assert_has_headroom(lo);
}

#[tokio::test]
async fn saturating_deadline_caps_at_thirty_years() {
    let base = Instant::now();
    assert_eq!(saturating_deadline(base, Duration::MAX), base + FAR_FUTURE);
    assert_eq!(
        saturating_deadline(base, Duration::from_secs(u64::MAX)),
        base + FAR_FUTURE
    );
}

// - route_report ---------------------------------------------------------

use crate::actor::supervision::{slot_pair, ChildSpec};

/// A supervisor whose OneForAll chain pends over `queue`, with the
/// children `lives` names in the registry and a budget of one restart.
fn a_chain(queue: &[&str], lives: &[(&str, ChildLifecycle)]) -> SupervisionState {
    let mut sup = SupervisionState::new(SupervisionConfig {
        strategy: RestartStrategy::OneForAll,
        max_restarts: 1,
        restart_window: Duration::from_secs(60),
        ..SupervisionConfig::default()
    });
    for (id, life) in lives {
        let (ctl, _io) = slot_pair(Default::default());
        sup.registry.register(ChildState {
            id: ActorId::from(*id),
            name: Some(Arc::from((*id).to_string())),
            spec: ChildSpec {
                restart_type: RestartType::Permanent,
                shutdown: Shutdown::default(),
                start_timeout: None,
                kill_grace: DEFAULT_KILL_GRACE,
                is_supervisor: false,
                significant: false,
                sys: crate::actor::sys::SysOptions::default(),
            },
            ctl,
            lifecycle: *life,
            origin: Origin::Chain,
            predecessor: None,
        });
    }
    sup.pending_group = Some(GroupPhase::Restarting {
        queue: queue.iter().map(|id| ActorId::from(*id)).collect(),
        overrides: HashMap::new(),
        started: HashMap::new(),
        scope: GroupScope::default(),
    });
    sup
}

fn down(n: u64) -> ChildLifecycle {
    ChildLifecycle::Down {
        incarnation: n,
        pending: DownPending::None,
    }
}

fn crash_of(id: &str, incarnation: u64) -> ChildStoppedInternal {
    ChildStoppedInternal {
        child_id: ActorId::from(id),
        reason: StopReason::Failure(ActorError::user("crashed")),
        incarnation,
        ignored: false,
    }
}
