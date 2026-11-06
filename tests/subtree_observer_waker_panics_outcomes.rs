//! A task that waits on a `SubtreeObserver` polls with a waker of its own. The
//! crate wakes that waker on the task's behalf: from the observed actor's own
//! task when its status closes, from the task of an actor below it when that
//! actor ends, and from the supervisor's task when a restart replaces a child.
//! A panic of the wake stays where the waker is woken: the observed actor ends
//! with its own reason, a supervisor still starts the restart, no outcome of
//! the subtree changes, and the observer answers on a later poll.
//!
//! The observer is polled by hand with a waker that panics in its wake while
//! it is armed, so the panic is raised in the task that ends or releases
//! something. Every wait carries its own bound, and no step is ordered by a
//! sleep: a bound only reports a hang or marks the window in which an answer
//! must not come. The wait for the observed actor's end is a `wait_stopped`
//! polled by hand, first on the actor's status list (tokio wakes a list in
//! the order its waiters registered, so a waker that panics strands only the
//! waiters behind it) and with a waker that counts and signals the test: the
//! test sees the end wake it, and a `timeout` around the wait would poll it
//! again at expiry and hide a lost wake.

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorSystem, StopReason, SubtreeEnd,
    SubtreeObserver, SupervisionConfig,
};

/// How long a call that must return takes at most.
const BOUND: Duration = Duration::from_secs(10);

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// How long an answer that must not come is given to come.
const PENDING: Duration = Duration::from_millis(100);

/// A kill grace no wait here comes near.
const FAR_GRACE: Duration = Duration::from_secs(30);

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

/// A waker that counts its wakes and panics in them while it is armed.
struct LoudWake {
    armed: AtomicBool,
    woken: AtomicUsize,
}

impl LoudWake {
    fn armed() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(true),
            woken: AtomicUsize::new(0),
        })
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }

    fn woken(&self) -> usize {
        self.woken.load(Ordering::SeqCst)
    }
}

impl Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a waker's wake panics");
        }
    }
}

/// A waker that counts its wakes, signals the test's wait and never panics.
/// With it a test sees the wake itself, and not the poll that a `timeout`
/// makes of its future when it expires.
#[derive(Default)]
struct SignalWake {
    woken: AtomicUsize,
    signal: Notify,
}

impl Wake for SignalWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        self.signal.notify_one();
    }
}

/// Polls `wait` once with `signal`'s waker.
fn poll_signal<F: Future + Unpin>(wait: &mut F, signal: &Arc<SignalWake>) -> Poll<F::Output> {
    let waker = Waker::from(signal.clone());
    Pin::new(wait).poll(&mut Context::from_waker(&waker))
}

/// Waits for `wait`, which has been polled with `signal`'s waker, to be woken
/// and to answer. A wake that does not come within `BOUND` fails the test: the
/// wait is polled only after a wake, never at the expiry of a bound.
async fn answered_on_a_wake<F: Future + Unpin>(
    wait: &mut F,
    signal: &Arc<SignalWake>,
) -> F::Output {
    loop {
        timeout(BOUND, signal.signal.notified())
            .await
            .expect("the wait was not woken: a wake was lost");
        if let Poll::Ready(answer) = poll_signal(wait, signal) {
            return answer;
        }
    }
}

/// Polls `wait` once with a fresh armed waker, which is kept in `wakers`.
fn poll_loud<F: Future + Unpin>(wait: &mut F, wakers: &mut Vec<Arc<LoudWake>>) -> Poll<F::Output> {
    let loud = LoudWake::armed();
    let waker = Waker::from(loud.clone());
    wakers.push(loud);
    Pin::new(wait).poll(&mut Context::from_waker(&waker))
}

/// Polls `wait` by hand with a fresh armed waker every few milliseconds until
/// it answers or `window` passes. No timer of the runtime is in the window: a
/// worker held inside a `Drop` can leave a runtime without a thread that
/// drives its timers.
async fn poll_loud_for<F: Future + Unpin>(
    window: Duration,
    wait: &mut F,
    wakers: &mut Vec<Arc<LoudWake>>,
) -> Option<F::Output> {
    let started = Instant::now();
    loop {
        if let Poll::Ready(answer) = poll_loud(wait, wakers) {
            return Some(answer);
        }
        if started.elapsed() >= window {
            return None;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Stops every waker from panicking and returns how many wakes they saw.
fn quiet(wakers: &[Arc<LoudWake>]) -> usize {
    wakers.iter().for_each(|loud| loud.disarm());
    wakers.iter().map(|loud| loud.woken()).sum()
}

/// A gate a `Drop` waits on.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *(self.0).0.lock().unwrap() = true;
        (self.0).1.notify_all();
    }

    /// Blocks the calling thread until the gate opens, or until the hang bound
    /// passes.
    fn wait(&self) {
        let (lock, changed) = &*self.0;
        let started = Instant::now();
        let mut open = lock.lock().unwrap();
        while !*open {
            let left = HANG_BOUND.saturating_sub(started.elapsed());
            if left.is_zero() {
                return;
            }
            open = changed.wait_timeout(open, left).unwrap().0;
        }
    }
}

/// Opens the gate when dropped, also while a failed assertion unwinds.
struct OpenOnDrop(Gate);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// An actor that spawns children, crashes on command and counts its starts.
/// While it holds a gate, its `Drop` waits on it.
struct Node {
    gate: Option<Gate>,
    starts: Option<Arc<AtomicUsize>>,
}

impl Node {
    fn plain() -> Self {
        Self {
            gate: None,
            starts: None,
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.take() {
            gate.wait();
        }
    }
}

enum Cmd {
    /// Spawn a child whose `Drop` waits on the gate.
    SpawnGated(String, Gate),
    /// Spawn a child that counts its starts.
    SpawnCounted(String, Arc<AtomicUsize>),
    /// Panic.
    Crash,
}

impl Actor for Node {
    type Message = Cmd;
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(starts) = &self.starts {
            starts.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn handle(&mut self, cmd: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match cmd {
            Cmd::SpawnGated(name, gate) => {
                ctx.spawn_child(move || Node {
                    gate: Some(gate.clone()),
                    starts: None,
                })
                .named(name)
                .kill_grace(FAR_GRACE)
                .await?;
                Ok(())
            }
            Cmd::SpawnCounted(name, starts) => {
                ctx.spawn_child(move || Node {
                    gate: None,
                    starts: Some(starts.clone()),
                })
                .named(name)
                .kill_grace(FAR_GRACE)
                .await?;
                Ok(())
            }
            Cmd::Crash => panic!("the node crashed on command"),
        }
    }
}

/// A root supervisor with a restart budget no test comes near.
async fn root(sys: &Arc<ActorSystem>) -> ActorHandle<Node> {
    Node::plain()
        .spawn()
        .named(unique("boss"))
        .on_system(sys)
        .kill_grace(Duration::from_millis(120))
        .with_config(ActorConfig::default().supervisor())
        .with_supervision(
            SupervisionConfig::one_for_one().max_restarts(20, Duration::from_secs(60)),
        )
        .await
        .expect("the root spawns")
}

fn new_system() -> Arc<ActorSystem> {
    ActorSystem::create(unique("waker-panics")).expect("the system is created")
}

fn is_the_crash(reason: &StopReason) -> bool {
    matches!(
        reason,
        StopReason::Failure(ActorError::Panic(text)) if text.contains("the node crashed on command")
    )
}

/// The observed actor has nothing below it. The observer waits for the
/// actor's own end with a waker that panics, and the actor ends with the
/// reason that was asked of it, a stop or its own crash.
#[test]
fn an_observer_whose_waker_panics_does_not_change_how_the_observed_actor_ends() {
    common::run_multi_thread_only(2, async {
        for crash in [false, true] {
            let actor = Node::plain().spawn().await.unwrap();
            // Parked first on the actor's status list, ahead of the observer.
            let signal = Arc::new(SignalWake::default());
            let mut stopped = Box::pin(actor.wait_stopped());
            assert!(
                poll_signal(&mut stopped, &signal).is_pending(),
                "crash {crash}: wait_stopped waits while the actor runs"
            );
            let observer = actor.observe_subtree();
            let mut wait = Box::pin(observer.wait_ended());
            let mut wakers = Vec::new();
            assert!(
                poll_loud(&mut wait, &mut wakers).is_pending(),
                "crash {crash}: the observer waits while the actor runs"
            );

            if crash {
                actor.notify(Cmd::Crash).await.unwrap();
            } else {
                timeout(BOUND, actor.stop(StopReason::Graceful))
                    .await
                    .expect("the stop returns")
                    .expect("the stop is accepted");
            }
            let reason = answered_on_a_wake(&mut stopped, &signal).await;
            if crash {
                assert!(
                    is_the_crash(&reason),
                    "an observer's waker that panicked changed how the actor ended: {reason:?}"
                );
            } else {
                assert_eq!(
                    reason,
                    StopReason::Graceful,
                    "an observer's waker that panicked changed how the actor ended"
                );
            }

            let end = poll_loud_for(HANG_BOUND, &mut wait, &mut wakers)
                .await
                .expect("the observer answers once the actor is gone");
            assert_eq!(end, SubtreeEnd::Ended, "crash {crash}");
            assert!(
                quiet(&wakers) >= 1,
                "crash {crash}: the observer's waker was woken"
            );
        }
    });
}

/// A boss ends while a leaf below it is held in its `Drop`. The observer is
/// polled all along with wakers that panic: they are woken by the boss's end,
/// by the release of its hold and by the leaf's end, and the kill, the leaf's
/// end and the answer are the same as without them.
#[test]
fn an_observer_whose_waker_panics_still_answers_ended_once_the_subtree_is_gone() {
    common::run_multi_thread_only(2, async {
        for round in 0..3 {
            let sys = new_system();
            let gate = Gate::default();
            let _open = OpenOnDrop(gate.clone());
            let boss = root(&sys).await;
            boss.send(Cmd::SpawnGated(unique("leaf"), gate.clone()))
                .await
                .expect("the leaf spawns");
            let id = boss.id().clone();
            let observer: SubtreeObserver =
                sys.observe_subtree(&id).expect("the boss is registered");
            let mut wait = Box::pin(observer.wait_ended());
            let mut wakers = Vec::new();
            assert!(
                poll_loud(&mut wait, &mut wakers).is_pending(),
                "round {round}: the observer waits while the boss runs"
            );

            let report = timeout(BOUND, sys.kill_by_id(&id))
                .await
                .expect("kill_by_id returns while the leaf is held")
                .expect("the boss was registered");
            assert_eq!(
                report.outcome,
                tokio_actors::StopOutcome::Killed,
                "round {round}: a waker that panicked changed how the boss ended"
            );

            assert!(
                poll_loud_for(PENDING, &mut wait, &mut wakers)
                    .await
                    .is_none(),
                "round {round}: the observer answered while the leaf was still running"
            );
            gate.open();
            let end = poll_loud_for(HANG_BOUND, &mut wait, &mut wakers)
                .await
                .expect("the observer answers once the leaf is gone");
            assert_eq!(end, SubtreeEnd::Ended, "round {round}");
            assert!(quiet(&wakers) >= 1, "round {round}: a waker was woken");
            assert!(
                sys.actor_ids().is_empty(),
                "round {round}: every actor of the subtree left the registry"
            );
        }
    });
}

/// A supervised child crashes while an observer of its incarnation waits with
/// a waker that panics. The child ends with its own reason, the supervisor
/// starts the next incarnation, and the observer reads the end of the first.
#[test]
fn an_observer_whose_waker_panics_does_not_stop_a_supervisor_from_restarting_the_child() {
    common::run_multi_thread_only(2, async {
        let sys = new_system();
        let boss = root(&sys).await;
        let starts = Arc::new(AtomicUsize::new(0));
        let name = unique("child");
        boss.send(Cmd::SpawnCounted(name.clone(), starts.clone()))
            .await
            .expect("the child spawns");
        let child = sys.get::<Node>(&name).expect("the child is registered");
        let started = Instant::now();
        while starts.load(Ordering::SeqCst) != 1 {
            assert!(started.elapsed() < HANG_BOUND, "the first start");
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(2));
        }

        // Parked first on the child's status list, ahead of the observer.
        let signal = Arc::new(SignalWake::default());
        let mut stopped = Box::pin(child.wait_stopped());
        assert!(
            poll_signal(&mut stopped, &signal).is_pending(),
            "wait_stopped waits while the child runs"
        );
        let observer = child.observe_subtree();
        let mut wait = Box::pin(observer.wait_ended());
        let mut wakers = Vec::new();
        assert!(
            poll_loud(&mut wait, &mut wakers).is_pending(),
            "the observer waits while the child runs"
        );

        child.notify(Cmd::Crash).await.unwrap();
        let reason = answered_on_a_wake(&mut stopped, &signal).await;
        assert!(
            is_the_crash(&reason),
            "an observer's waker that panicked changed how the child ended: {reason:?}"
        );

        // The observer may answer for the first incarnation before the second
        // one starts; its answer is kept, and the future is not polled again.
        let mut end = None;
        let started = Instant::now();
        while starts.load(Ordering::SeqCst) != 2 {
            assert!(
                started.elapsed() < HANG_BOUND,
                "the supervisor did not start the next incarnation"
            );
            if end.is_none() {
                if let Poll::Ready(answer) = poll_loud(&mut wait, &mut wakers) {
                    end = Some(answer);
                }
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(2));
        }

        let end = match end {
            Some(end) => end,
            None => poll_loud_for(HANG_BOUND, &mut wait, &mut wakers)
                .await
                .expect("the observer answers for the first incarnation"),
        };
        assert_eq!(end, SubtreeEnd::Ended);
        assert!(quiet(&wakers) >= 1, "a waker was woken");
        assert_eq!(starts.load(Ordering::SeqCst), 2, "one restart");
    });
}
