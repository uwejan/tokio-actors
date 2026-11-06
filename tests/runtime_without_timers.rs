//! A call that bounds its wait sleeps on the calling runtime, so on a runtime
//! built without the Tokio time driver it panics at its entry, before it
//! raises, spawns or registers anything. Plain actors, spawn included, run on
//! such a runtime (see the facts in `timerless_facts.rs`).

use std::future::{Future, IntoFuture};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use tokio::runtime::{Builder, Runtime};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorStatus, ActorSystem, SpawnError,
};

#[derive(Default)]
struct Noop;

impl Actor for Noop {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn without_timers() -> Runtime {
    Builder::new_current_thread().build().unwrap()
}

fn with_timers() -> Runtime {
    Builder::new_current_thread().enable_all().build().unwrap()
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

struct Counting {
    started: &'static std::sync::atomic::AtomicUsize,
}

impl Actor for Counting {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// The text of the panic a spawned call ended with.
fn panic_text(joined: Result<impl Sized, tokio::task::JoinError>) -> String {
    match joined {
        Ok(_) => panic!("the call was expected to panic"),
        Err(err) => *err
            .into_panic()
            .downcast::<String>()
            .expect("a formatted panic message"),
    }
}

#[test]
fn a_spawn_with_a_start_timeout_panics_before_the_actor_exists_on_a_runtime_without_the_driver() {
    static STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let sys = ActorSystem::create(unique("no-timers-start")).unwrap();
    let caller = without_timers();
    let message = caller.block_on(async {
        let sys = sys.clone();
        let call = tokio::spawn(async move {
            Counting { started: &STARTED }
                .spawn()
                .named("worker")
                .on_system(&sys)
                .start_timeout(std::time::Duration::from_secs(5))
                .await
                .map(|_| ())
        });
        let message = panic_text(call.await);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        message
    });
    assert_eq!(
        message,
        "tokio-actors: SpawnBuilder::start_timeout needs the Tokio time driver on the calling \
         runtime (Builder::enable_time or enable_all)"
    );
    assert!(sys.actor_ids().is_empty(), "nothing was registered");
    assert!(sys.registered().is_empty());
    assert_eq!(
        STARTED.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no task was spawned"
    );
}

/// Runs `spawn` as a task of a runtime without the time driver and returns
/// what it answered.
fn spawned_without_timers<F>(spawn: F) -> Result<(), SpawnError>
where
    F: Future<Output = Result<(), SpawnError>> + Send + 'static,
{
    let caller = without_timers();
    caller.block_on(async move { tokio::spawn(spawn).await.expect("the spawn did not panic") })
}

/// A name already taken is answered with the error a spawn gives without a
/// bound, before the timer check.
#[test]
fn a_timed_spawn_under_a_taken_name_is_refused_on_a_runtime_without_the_driver() {
    let sys = ActorSystem::create(unique("no-timers-taken")).unwrap();
    let host = with_timers();
    let first = host.block_on({
        let sys = sys.clone();
        async move { Noop.spawn().named("worker").on_system(&sys).await }
    });
    let first = first.unwrap();
    let answer = spawned_without_timers({
        let sys = sys.clone();
        async move {
            Noop.spawn()
                .named("worker")
                .on_system(&sys)
                .start_timeout(std::time::Duration::from_secs(5))
                .await
                .map(|_| ())
        }
    });
    assert_eq!(
        answer.err(),
        Some(SpawnError::NameTaken {
            name: "worker".to_string(),
            system: sys.name().to_string(),
        })
    );
    assert_eq!(
        sys.registered(),
        vec!["worker".to_string()],
        "the first owner is untouched"
    );
    assert_eq!(first.status(), ActorStatus::Running);
}

/// A system that no longer takes registrations is answered with
/// `SystemShuttingDown` before the timer check.
#[test]
fn a_timed_spawn_into_a_shutting_down_system_is_refused_on_a_runtime_without_the_driver() {
    let sys = ActorSystem::create(unique("no-timers-down")).unwrap();
    let host = with_timers();
    host.block_on({
        let sys = sys.clone();
        async move {
            let _ = sys.shutdown().await;
        }
    });
    let answer = spawned_without_timers({
        let sys = sys.clone();
        async move {
            Noop.spawn()
                .on_system(&sys)
                .start_timeout(std::time::Duration::from_secs(5))
                .await
                .map(|_| ())
        }
    });
    assert_eq!(
        answer.err(),
        Some(SpawnError::SystemShuttingDown(sys.name().to_string()))
    );
}

/// A zero mailbox capacity is answered as any spawn answers it.
#[test]
fn a_timed_spawn_with_a_zero_mailbox_is_refused_on_a_runtime_without_the_driver() {
    let answer = spawned_without_timers(async move {
        Noop.spawn()
            .with_config(tokio_actors::ActorConfig::default().with_mailbox_capacity(0))
            .start_timeout(std::time::Duration::from_secs(5))
            .await
            .map(|_| ())
    });
    assert_eq!(answer.err(), Some(SpawnError::ZeroMailboxCapacity));
}

#[test]
fn a_spawn_with_a_start_timeout_and_no_runtime_current_is_refused_as_any_spawn_is() {
    let mut spawn = pin!(Noop
        .spawn()
        .start_timeout(std::time::Duration::from_secs(5))
        .into_future());
    let mut cx = Context::from_waker(Waker::noop());
    match spawn.as_mut().poll(&mut cx) {
        Poll::Ready(result) => assert_eq!(result.err(), Some(SpawnError::MissingRuntime)),
        Poll::Pending => panic!("a spawn with no runtime current is answered at once"),
    }
}

#[test]
fn a_spawn_with_no_runtime_current_is_refused() {
    let mut spawn = pin!(Noop.spawn().into_future());
    let mut cx = Context::from_waker(Waker::noop());
    match spawn.as_mut().poll(&mut cx) {
        Poll::Ready(result) => assert_eq!(result.err(), Some(SpawnError::MissingRuntime)),
        Poll::Pending => panic!("a spawn with no runtime current is answered at once"),
    }
}

#[test]
fn kill_by_id_answers_none_for_an_id_nothing_holds_on_any_calling_runtime() {
    let sys = ActorSystem::create(unique("no-timers-kill-unknown")).unwrap();
    let caller = without_timers();
    let outcome = caller.block_on(async {
        let sys = sys.clone();
        tokio::spawn(async move { sys.kill_by_id(&"nobody".into()).await }).await
    });
    assert!(outcome.expect("the call does not panic").is_none());
    let nobody = tokio_actors::types::ActorId::from("nobody");
    let mut call = pin!(sys.kill_by_id(&nobody));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(call.as_mut().poll(&mut cx), std::task::Poll::Ready(None)),
        "with no runtime current the answer is the same"
    );
}
