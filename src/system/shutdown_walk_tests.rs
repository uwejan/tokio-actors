//! The shutdown walk and the sweep deadline the system keeps for it: a walk
//! cut short after its sweep began still has its leftovers killed at the
//! deadline and aborted a grace later; shutdowns that are all cut short keep
//! the deadline, and a shutdown made after them folds into it; overlapping
//! calls each run their own walk.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::*;
use crate::actor::context::ActorContext;
use crate::actor::ActorExt;
use crate::error::ActorResult;
use crate::types::{StatusValue, StopOutcome};

/// The longest a wait may take. It only limits how long a real hang takes to
/// report; a slow host must not read as one.
const BOUND: Duration = Duration::from_secs(30);

fn unique(what: &str) -> String {
    format!("{what}-{}", uuid::Uuid::new_v4())
}

/// Does nothing.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A non-root registration that nothing stops: its task holds the lane, the
/// status plane and the registry guard and parks for ever, so only an abort
/// ends it, and its end closes the plane without a `Stopped` write.
struct Leftover {
    lane: StopLane,
    task: tokio::task::JoinHandle<()>,
}

impl Leftover {
    fn register(sys: &Arc<ActorSystem>, id: &ActorId, grace: Duration) -> Leftover {
        let (tx, _rx) = crate::actor::mailbox::test_channel(1);
        let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
        let (stop_lane, stop_rx) = StopLane::new();
        let handle = ActorHandle::<Idle>::new(
            id.clone(),
            tx,
            stop_lane.clone(),
            1,
            status_rx,
            sys.name().into(),
            None,
            Some(Arc::new(AtomicBool::new(false))),
        );
        let seq = sys
            .register_actor::<Idle>(
                id,
                None,
                &handle,
                false,
                TurnClock::default(),
                grace,
                Handle::current(),
            )
            .unwrap();
        let guard = RegistryGuard::new(sys.clone(), id.clone(), None, seq);
        let task = tokio::spawn(async move {
            let _owned = (stop_rx, status_tx, guard);
            std::future::pending::<()>().await;
        });
        sys.attach_abort(id, None, task.abort_handle(), seq);
        Leftover {
            lane: stop_lane,
            task,
        }
    }
}

fn phase_of(sys: &ActorSystem) -> SystemPhase {
    SystemPhase::from_u8(sys.phase.load(Ordering::Acquire))
}

/// Ends `rt` and returns once every task on it is dropped.
fn end_runtime(rt: tokio::runtime::Runtime) {
    std::thread::spawn(move || drop(rt))
        .join()
        .expect("the runtime ended cleanly");
}

// - A walk cut short after its sweep began ----------------------------------

/// A shutdown whose walk runs on a runtime that ends once the sweep has
/// begun still has the system kill the leftover at the deadline and abort it
/// a kill grace later, because both were scheduled with the system when the
/// sweep began.
async fn a_walk_cut_short_after_the_sweep_began_still_ends_its_leftover() {
    let sys = ActorSystem::create(unique("cut-sweep")).unwrap();
    let id = ActorId::from("cut-sweep-leftover");
    let leftover = Leftover::register(&sys, &id, Duration::from_millis(50));
    let host = Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    // The window is far longer than ending the runtime takes, so the walk
    // cannot force the leftover itself before it is cut short.
    drop(host.spawn({
        let sys = sys.clone();
        async move {
            sys.shutdown_with(ShutdownPolicy {
                timeout: Duration::from_secs(3),
                per_actor_timeout: Duration::from_secs(1),
            })
            .await
        }
    }));
    let deadline = {
        let mut held = sys.sweep_deadline.subscribe();
        let seen = timeout(BOUND, held.wait_for(|deadline| deadline.is_some()))
            .await
            .expect("the sweep began within the bound")
            .expect("the system holds its sweep deadline");
        (*seen).expect("the deadline is held")
    };
    end_runtime(host);
    assert!(
        Instant::now() < deadline,
        "the walk was cut short before the deadline"
    );
    assert_eq!(leftover.lane.reason(), None, "nothing has been forced yet");

    let ended = timeout(BOUND, leftover.task)
        .await
        .expect("the leftover's task was aborted within the bound");
    assert!(ended.unwrap_err().is_cancelled(), "its end is the abort");
    assert!(
        matches!(leftover.lane.reason(), Some(StopReason::Kill)),
        "the abort came after the deadline's Kill"
    );
    assert!(
        Instant::now() >= deadline,
        "neither stage ran before the deadline"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_walk_cut_short_after_the_sweep_began_still_ends_its_leftover_multi_thread() {
    a_walk_cut_short_after_the_sweep_began_still_ends_its_leftover().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_walk_cut_short_after_the_sweep_began_still_ends_its_leftover_current_thread() {
    a_walk_cut_short_after_the_sweep_began_still_ends_its_leftover().await;
}

// - Cut-short shutdowns keep the sweep deadline -----------------------------

/// Shutdowns that are all cut short leave the sweep deadline they folded in,
/// and a shutdown made after them folds into it: the stored deadline has
/// passed, so that shutdown forces its leftover at once, and its completion
/// clears the deadline.
#[tokio::test(start_paused = true)]
async fn all_cut_short_shutdowns_keep_the_sweep_deadline_and_a_later_one_folds_into_it() {
    let sys = ActorSystem::create(unique("episode-deadline")).unwrap();
    let leftover = Leftover::register(
        &sys,
        &ActorId::from("episode-deadline-leftover"),
        Duration::from_millis(50),
    );

    let mut first = ShutdownInFlight::enter(sys.clone());
    let held = sys.fold_sweep_deadline(Instant::now() + Duration::from_secs(1));
    first.completed = false;
    drop(first);
    assert_eq!(
        *sys.sweep_deadline.borrow(),
        Some(held),
        "a shutdown that was cut short leaves the deadline"
    );
    assert_eq!(phase_of(&sys), SystemPhase::ShuttingDown);

    // The stored deadline has passed when the next shutdown is made.
    tokio::time::advance(Duration::from_secs(5)).await;
    let started = Instant::now();
    let report = timeout(
        BOUND,
        sys.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_secs(90),
            per_actor_timeout: Duration::from_secs(10),
        }),
    )
    .await
    .expect("the later shutdown returned within the bound");
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "the stored deadline had passed, so the leftover was forced at once, not at its own 90 s: {took:?}"
    );
    assert_eq!(report.swept.len(), 1, "the leftover is reported once");
    assert_ne!(
        report.swept[0].1.outcome,
        StopOutcome::Graceful,
        "the leftover was forced"
    );
    assert!(
        matches!(leftover.lane.reason(), Some(StopReason::Kill)),
        "the force raised its Kill"
    );
    assert_eq!(
        *sys.sweep_deadline.borrow(),
        None,
        "the completed shutdown clears the deadline with the move to Defunct"
    );
    assert_eq!(phase_of(&sys), SystemPhase::Defunct);
}

// - Overlapping calls each run their own walk --------------------------------

/// Vetoes a stop request while `veto` is set, and reports each request.
struct Veto {
    veto: Arc<AtomicBool>,
    asked: mpsc::UnboundedSender<()>,
}

impl Actor for Veto {
    type Message = ();
    type Response = ();

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        let _ = self.asked.send(());
        !self.veto.load(Ordering::SeqCst)
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn walk_ends(sys: &ActorSystem) -> Vec<crate::helper::HelperEnd> {
    sys.helper_ends(&Handle::current())
        .into_iter()
        .filter(|(kind, _)| *kind == HelperKind::ShutdownWalk)
        .map(|(_, end)| end)
        .collect()
}

/// A second call made while the first is still waiting on a root runs a walk
/// of its own: two walk helpers end, each finished, and each call reports the
/// root.
async fn overlapping_calls_each_run_their_own_walk() {
    let sys = ActorSystem::create(unique("two-walks")).unwrap();
    let veto = Arc::new(AtomicBool::new(true));
    let (asked_tx, mut asked_rx) = mpsc::unbounded_channel();
    let root = Veto {
        veto: veto.clone(),
        asked: asked_tx,
    }
    .spawn()
    .named("two-walks-root")
    .on_system(&sys)
    .await
    .unwrap();
    let policy = || ShutdownPolicy {
        timeout: Duration::from_secs(20),
        per_actor_timeout: Duration::from_secs(10),
    };

    let first = tokio::spawn({
        let sys = sys.clone();
        async move { sys.shutdown_with(policy()).await }
    });
    timeout(BOUND, asked_rx.recv())
        .await
        .expect("the first walk asked the root within the bound")
        .expect("the root reports its request");
    assert!(
        walk_ends(&sys).is_empty(),
        "the first walk is still waiting on the root"
    );

    // The root accepts the next request, which is the second walk's.
    veto.store(false, Ordering::SeqCst);
    let second = tokio::spawn({
        let sys = sys.clone();
        async move { sys.shutdown_with(policy()).await }
    });
    let (first, second) = timeout(BOUND, async { tokio::join!(first, second) })
        .await
        .expect("both calls returned within the bound");
    for report in [first.unwrap(), second.unwrap()] {
        assert_eq!(report.outcomes.len(), 1, "the root is in each report");
        assert_eq!(report.outcomes[0].0, *root.id());
    }
    timeout(BOUND, async {
        while walk_ends(&sys).len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("two walk helper ends were recorded");
    assert_eq!(
        walk_ends(&sys),
        vec![crate::helper::HelperEnd::Finished; 2],
        "each call ran a walk of its own"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlapping_calls_each_run_their_own_walk_multi_thread() {
    overlapping_calls_each_run_their_own_walk().await;
}

#[tokio::test(flavor = "current_thread")]
async fn overlapping_calls_each_run_their_own_walk_current_thread() {
    overlapping_calls_each_run_their_own_walk().await;
}

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: std::sync::atomic::AtomicUsize,
}

impl std::task::Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a waker's wake panics");
        }
    }
}

/// The caller of a shutdown waits on the walk's report with a waker that
/// panics when it is woken. The walk's helper sends the report: the panic
/// stays out of the helper, which ends clean, and the caller reads the report
/// on its next poll.
#[tokio::test(flavor = "current_thread")]
async fn a_caller_whose_waker_panics_when_the_walk_reports_leaves_the_walk_clean() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    let sys = ActorSystem::create(unique("loud-report")).unwrap();
    let loud = Arc::new(LoudWake::default());
    loud.armed.store(true, Ordering::SeqCst);
    let waker = Waker::from(loud.clone());
    let mut call = Box::pin(sys.shutdown());
    assert!(call
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());

    timeout(BOUND, async {
        while loud.woken.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the walk reported to the waiting caller");
    loud.armed.store(false, Ordering::SeqCst);

    timeout(BOUND, async {
        while sys.live_helpers() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the walk helper is counted down");
    assert_eq!(
        walk_ends(&sys),
        vec![crate::helper::HelperEnd::Finished],
        "a caller's waker ended the walk helper"
    );
    assert!(matches!(
        call.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(_)
    ));
}

/// The walk's helper panics before it reports, and the caller of the shutdown
/// waits for the report with a waker that panics when it is woken. Dropping the
/// report channel while the walk unwinds wakes the caller; a panic that escaped
/// that drop would be a second panic during the unwind and end the process, so
/// the scenario runs in a child process. Contained, the caller walks the system
/// itself and returns that report.
#[test]
fn a_caller_whose_waker_panics_when_a_panicked_walk_drops_the_report_does_not_end_the_process() {
    use std::future::Future;
    use std::task::{Context, Waker};

    if !crate::actor::panic::in_a_child_process(
        module_path!(),
        "a_caller_whose_waker_panics_when_a_panicked_walk_drops_the_report_does_not_end_the_process",
    ) {
        return;
    }
    let runtime = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let sys = ActorSystem::create(unique("loud-walk")).unwrap();
        let root = Idle
            .spawn()
            .named(unique("loud-walk-root"))
            .on_system(&sys)
            .await
            .unwrap();
        let armed = Arc::new(AtomicBool::new(true));
        let hook_armed = armed.clone();
        sys.set_ladder_hook(move |point, _id| {
            if point == LadderPoint::Stop && hook_armed.swap(false, Ordering::SeqCst) {
                panic!("the walk panics once");
            }
        });
        let loud = Arc::new(LoudWake::default());
        loud.armed.store(true, Ordering::SeqCst);
        let waker = Waker::from(loud.clone());
        let mut call = Box::pin(sys.shutdown());
        assert!(call
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        timeout(BOUND, async {
            while loud.woken.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the panicked walk dropped the report channel");
        loud.armed.store(false, Ordering::SeqCst);

        let report = timeout(BOUND, call)
            .await
            .expect("the caller walks the system itself");
        assert!(!armed.load(Ordering::SeqCst), "the first walk panicked");
        assert!(
            report
                .outcomes
                .iter()
                .any(|(id, report)| id == root.id() && report.outcome == StopOutcome::Graceful),
            "the root is reported stopped: {report:?}"
        );
    });
}
