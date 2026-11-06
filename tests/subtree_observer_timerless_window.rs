//! A caller that cannot give `SubtreeObserver::wait_ended` a timer, calling it
//! while the actor it waits for ends.
//!
//! The wait for an actor's own end needs no timer. The status plane of an
//! actor closes a step before the actor lets go of its hold on its subtree,
//! and a call that finds the plane closed goes on to wait for the hold, so an
//! actor with nothing below it is answered `Ended` on a runtime without the
//! time driver however the two races. Each test ends actors at the moment the
//! call is made, and counts the calls that panic: none may.
//!
//! Each scenario runs on a thread of its own under a real-time deadline, so a
//! call that never answers fails the test and not the run.

use std::any::Any;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, StopReason, SubtreeEnd, SubtreeObserver,
};

/// Rounds of each scenario: enough that a window of under one round in a
/// hundred is met several times over, and few enough to end in seconds.
const ROUNDS: usize = 3000;

/// The longest a scenario may take. It only limits how long a hang takes to
/// report.
const DEADLINE: Duration = Duration::from_secs(120);

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

/// An actor with no timer, no stream and no child.
struct Plain;

impl Actor for Plain {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// What the calls of one scenario ended with.
#[derive(Default)]
struct Tally {
    ended: usize,
    panicked: usize,
    first_panic: Option<String>,
}

impl Tally {
    /// Makes the call on `caller` and counts how it ended.
    fn call(&mut self, caller: &Runtime, observer: SubtreeObserver) {
        match catch_unwind(AssertUnwindSafe(|| caller.block_on(observer.wait_ended()))) {
            Ok(SubtreeEnd::Ended) => self.ended += 1,
            Ok(other) => panic!("an actor with nothing below it was answered {other:?}"),
            Err(payload) => {
                self.panicked += 1;
                self.first_panic.get_or_insert_with(|| text_of(payload));
            }
        }
    }

    fn assert_none_panicked(&self, what: &str) {
        assert_eq!(
            self.panicked,
            0,
            "{what}: {} of {ROUNDS} calls panicked, the first with: {}",
            self.panicked,
            self.first_panic.as_deref().unwrap_or("")
        );
        assert_eq!(self.ended, ROUNDS, "{what}: every call answers Ended");
    }
}

fn text_of(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => payload.downcast_ref::<&str>().map_or_else(
            || "a panic with no text".to_string(),
            |text| text.to_string(),
        ),
    }
}

/// Runs `scenario` on a thread of its own and returns its tally, failing the
/// test when it does not end within the deadline.
fn within_deadline(what: &str, scenario: impl FnOnce() -> Tally + Send + 'static) -> Tally {
    let (done, answer) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name(what.to_string())
        .spawn(move || {
            let _ = done.send(scenario());
        })
        .expect("the scenario thread starts");
    match answer.recv_timeout(DEADLINE) {
        Ok(tally) => {
            thread.join().expect("the scenario thread ends");
            tally
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("{what}: no end within {DEADLINE:?}, a call never answered")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => match thread.join() {
            Err(payload) => resume_unwind(payload),
            Ok(()) => panic!("{what}: the scenario ended without a tally"),
        },
    }
}

/// The actors run on a runtime with the time driver and are killed by id from
/// it; the caller waits on a current-thread runtime built without the time
/// driver.
#[test]
fn a_caller_without_a_time_driver_is_answered_ended_for_an_actor_killed_by_id_as_it_waits() {
    let tally = within_deadline("killed-by-id", || {
        let actors = Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("the actors' runtime");
        let caller = Builder::new_current_thread()
            .build()
            .expect("the caller's runtime");
        let sys = actors.block_on(async { ActorSystem::create(unique("window-kill")).unwrap() });
        let mut tally = Tally::default();
        for round in 0..ROUNDS {
            let (handle, observer) = actors.block_on(async {
                let handle = Plain
                    .spawn()
                    .named(format!("plain-{round}"))
                    .on_system(&sys)
                    .await
                    .expect("the actor spawns");
                let observer = handle.observe_subtree();
                (handle, observer)
            });
            let id = handle.id().clone();
            let killer = actors.spawn({
                let sys = sys.clone();
                async move {
                    tokio::task::yield_now().await;
                    sys.kill_by_id(&id).await
                }
            });
            tally.call(&caller, observer);
            actors.block_on(async {
                let _ = killer.await;
            });
        }
        tally
    });
    tally.assert_none_panicked("kill_by_id then wait_ended without a time driver");
}

/// One multi-thread runtime without the time driver hosts the actors and the
/// caller; each actor gets `reason` as the call is made, and the plain actor
/// spawned with `grace` for its kill grace.
fn stopped_on_one_runtime(
    what: &'static str,
    reason: StopReason,
    grace: Option<Duration>,
) -> Tally {
    within_deadline(what, move || {
        let rt = Builder::new_multi_thread()
            .worker_threads(4)
            .build()
            .expect("a runtime without the time driver");
        let sys = ActorSystem::create(unique(what)).unwrap();
        let mut tally = Tally::default();
        for round in 0..ROUNDS {
            let (handle, observer) = rt.block_on(async {
                let spawn = Plain
                    .spawn()
                    .named(format!("plain-{round}"))
                    .on_system(&sys);
                let handle = match grace {
                    Some(grace) => spawn.kill_grace(grace).await,
                    None => spawn.await,
                }
                .expect("the actor spawns");
                let observer = handle.observe_subtree();
                (handle, observer)
            });
            let reason = reason.clone();
            let stopper = rt.spawn(async move {
                tokio::task::yield_now().await;
                let _ = handle.stop(reason).await;
            });
            tally.call(&rt, observer);
            rt.block_on(async {
                let _ = stopper.await;
            });
        }
        tally
    })
}

/// A graceful stop: the actor ends by itself as the call is made.
#[test]
fn a_caller_on_a_runtime_without_a_time_driver_is_answered_ended_for_a_plain_actor_stopped_as_it_waits(
) {
    stopped_on_one_runtime("stopped", StopReason::Graceful, None)
        .assert_none_panicked("stop then wait_ended on a runtime without a time driver");
}

/// A `Kill` on a runtime without the time driver aborts the task at once, so
/// the abort is recorded beside the actor's own end.
#[test]
fn a_caller_on_a_runtime_without_a_time_driver_is_answered_ended_for_a_plain_actor_killed_as_it_waits(
) {
    stopped_on_one_runtime("killed", StopReason::Kill, Some(Duration::ZERO))
        .assert_none_panicked("kill then wait_ended on a runtime without a time driver");
}
