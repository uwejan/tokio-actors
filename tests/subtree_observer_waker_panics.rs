//! A subtree observer waits with a waker of its own, and the crate wakes that
//! waker when the subtree it observes changes. The restart of a supervised
//! child waits for the previous incarnation's whole subtree to end before it
//! starts the next one, on the same subtree. A panic of the observer's waker
//! stays where the crate wakes it and strands no wait of the crate: the restart
//! starts when the subtree ends, as it does with a waker that does not panic.

mod common;

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorHandle, ActorResult, ActorSystem, Shutdown, SubtreeEnd, SupervisionConfig,
};

/// How long a call that must return takes at most.
const BOUND: Duration = Duration::from_secs(10);

/// The longest a gate holds a thread. It only limits how long a real hang
/// takes to report.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// How long an event that must not happen is given to happen.
const SETTLE: Duration = Duration::from_millis(400);

/// A kill grace no wait here comes near: an abort after it cannot be what ends
/// an actor.
const FAR_GRACE: Duration = Duration::from_secs(30);

/// A gate that a `Drop` waits on, so the actor holding it ends only when the
/// test opens the gate.
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

/// Opens the gate when dropped, so a failed assertion never leaves a thread
/// blocked.
struct OpenOnDrop(Gate);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// A supervisor of the tree below. An actor that holds a gate waits on it when
/// its value is dropped, at the end of its task.
struct Node {
    gate: Option<Gate>,
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.take() {
            gate.wait();
        }
    }
}

enum Cmd {
    /// Spawns a supervisor child.
    Supervisor(String),
    /// Spawns a supervisor child whose orderly stop has no bound and whose end
    /// waits on the gate.
    Stuck(String, Gate),
    /// Enters the handler, says so, and stays in it until released.
    Park(Arc<Notify>, Arc<Notify>),
}

impl Actor for Node {
    type Message = Cmd;
    type Response = ();

    async fn handle(&mut self, cmd: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match cmd {
            Cmd::Supervisor(name) => {
                ctx.spawn_child(|| Node { gate: None })
                    .named(name)
                    .kill_grace(FAR_GRACE)
                    .with_config(ActorConfig::default().supervisor())
                    .await?;
            }
            Cmd::Stuck(name, gate) => {
                ctx.spawn_child(move || Node {
                    gate: Some(gate.clone()),
                })
                .named(name)
                .shutdown(Shutdown::Infinity)
                .kill_grace(FAR_GRACE)
                .with_config(ActorConfig::default().supervisor())
                .await?;
            }
            Cmd::Park(entered, release) => {
                entered.notify_one();
                release.notified().await;
            }
        }
        Ok(())
    }
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
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
            panic!("an observer's waker panics");
        }
    }
}

/// Kills a supervised child `mid` that has a stuck supervisor `stuck` below it,
/// while its supervisor `boss` is parked in a handler and takes no death. An
/// observer of `mid`'s subtree, polled with a waker that panics when armed,
/// waits for the subtree. The boss is then released and starts the restart of
/// `mid`, which waits for the same subtree to end. The gate opens with the
/// waker armed. Returns whether the next incarnation of `mid` started.
async fn restart_after_the_old_subtree_ends(arm_the_observers_waker: bool) -> bool {
    let sys = ActorSystem::create(unique("restart")).expect("the system is created");
    let gate = Gate::default();
    let _open = OpenOnDrop(gate.clone());
    let boss: ActorHandle<Node> = Node { gate: None }
        .spawn()
        .named(unique("boss"))
        .on_system(&sys)
        .kill_grace(Duration::from_millis(120))
        .with_supervision(
            SupervisionConfig::one_for_one().max_restarts(20, Duration::from_secs(60)),
        )
        .await
        .expect("the boss spawns");
    let mid_name = unique("mid");
    boss.send(Cmd::Supervisor(mid_name.clone()))
        .await
        .expect("mid spawns");
    let mid = sys.get::<Node>(&mid_name).expect("mid is registered");
    mid.send(Cmd::Stuck(unique("stuck"), gate.clone()))
        .await
        .expect("the stuck child spawns");

    // The boss takes no death while its handler is parked.
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    boss.notify(Cmd::Park(entered.clone(), release.clone()))
        .await
        .unwrap();
    timeout(BOUND, entered.notified())
        .await
        .expect("the boss is parked");

    let (_report, observer) = sys
        .kill_by_id_with_observer(mid.id())
        .await
        .expect("mid is registered");

    // The observer is polled until it waits on the subtree the stuck child
    // holds: the actor itself has ended, and an observer that waits for more
    // than that has the waker parked on the subtree's list.
    let loud = Arc::new(LoudWake::default());
    let waker = Waker::from(loud.clone());
    let mut cx = Context::from_waker(&waker);
    let mut waiting = Box::pin(observer.wait_ended());
    let parked = Instant::now();
    while parked.elapsed() < SETTLE {
        assert!(
            waiting.as_mut().poll(&mut cx).is_pending(),
            "the stuck child holds the subtree"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // The boss takes the death and starts the restart, which waits for the
    // same subtree with no deadline: a supervisor below it has no bound.
    release.notify_one();
    tokio::time::sleep(SETTLE).await;
    assert!(
        sys.get::<Node>(&mid_name).is_none(),
        "the restart waits for the old subtree"
    );

    loud.armed.store(arm_the_observers_waker, Ordering::SeqCst);
    gate.open();
    let restarted = timeout(BOUND, async {
        while sys.get::<Node>(&mid_name).is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok();
    loud.armed.store(false, Ordering::SeqCst);
    if restarted {
        // The observer finishes at its next poll, whether or not it was woken.
        let ended = timeout(BOUND, async {
            loop {
                match waiting.as_mut().poll(&mut cx) {
                    Poll::Ready(end) => break end,
                    Poll::Pending => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
        })
        .await
        .expect("the observer finishes");
        assert_eq!(ended, SubtreeEnd::Ended);
    }
    restarted
}

/// With a waker that does not panic, the restart starts when the old subtree
/// ends.
// The stuck child's drop blocks its thread, so the test needs another worker to
// see the restart wait: these two run on a multi-thread runtime only.
#[test]
fn a_restart_waits_for_the_old_subtree_and_starts_when_it_ends() {
    common::run_multi_thread_only(4, async {
        assert!(
            restart_after_the_old_subtree_ends(false).await,
            "the restart never started"
        );
    });
}

/// An observer of the old subtree whose waker panics when it is woken does not
/// keep the restart that waits for the same subtree from starting.
#[test]
fn a_panicking_observer_waker_does_not_strand_the_restart_waiting_for_the_same_subtree() {
    common::run_multi_thread_only(4, async {
        assert!(
            restart_after_the_old_subtree_ends(true).await,
            "the observer's waker kept the restart from starting"
        );
    });
}
