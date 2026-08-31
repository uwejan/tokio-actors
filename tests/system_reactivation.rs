//! Behavioral suite for system reactivation.
//!
//! `ActorSystem::shutdown`/`shutdown_with` move a system through a tri-state
//! phase: `Active` -> `ShuttingDown` (as soon as shutdown is called) ->
//! `Defunct` (once every root has stopped). Both non-`Active` phases reject
//! new registrations with `SpawnError::SystemShuttingDown`, but unlike a
//! one-way flag, a `Defunct` system is not poisoned forever:
//! `ActorSystem::reactivate` returns it to `Active`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, ShutdownPolicy, SpawnError,
};

#[derive(Default)]
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn uname(base: &str) -> String {
    format!("reactivation-{base}-{}", uuid::Uuid::new_v4())
}

#[tokio::test(flavor = "multi_thread")]
async fn defunct_system_rejects_registration_until_reactivated() {
    let sys = ActorSystem::create(uname("defunct")).unwrap();

    // An empty shutdown still runs the full sequence and lands the system in
    // Defunct once it completes.
    let report = sys.shutdown().await;
    assert!(report.outcomes.is_empty());
    assert!(report.swept.is_empty());

    let result = Idle.spawn().named("too-late").on_system(&sys).await;
    match result {
        Err(SpawnError::SystemShuttingDown(_)) => {}
        Ok(_) => panic!("a Defunct system must still reject registration"),
        Err(other) => panic!("expected SpawnError::SystemShuttingDown, got: {other}"),
    }

    assert!(
        sys.reactivate(),
        "a Defunct system must accept reactivation"
    );

    let handle = Idle
        .spawn()
        .named("post-reactivation")
        .on_system(&sys)
        .await
        .expect("registration must succeed once the system is Active again");
    assert!(handle.is_alive());
}

#[tokio::test(flavor = "multi_thread")]
async fn reactivate_on_a_never_shut_down_system_is_a_no_op_failure() {
    let sys = ActorSystem::create(uname("active")).unwrap();
    assert!(
        !sys.reactivate(),
        "reactivate on an Active system (never shut down) must report false"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reactivate_refused_while_shutdown_is_in_flight() {
    enum Msg {
        Hang,
    }

    #[derive(Default)]
    struct Hung {
        entered: Arc<AtomicBool>,
    }

    impl Actor for Hung {
        type Message = Msg;
        type Response = ();

        async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            match msg {
                Msg::Hang => {
                    self.entered.store(true, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                    unreachable!("pending() never resolves")
                }
            }
        }
    }

    let sys = ActorSystem::create(uname("mid-flight")).unwrap();

    let entered = Arc::new(AtomicBool::new(false));
    let handle = Hung {
        entered: entered.clone(),
    }
    .spawn()
    .named("hung-root")
    .on_system(&sys)
    .await
    .unwrap();
    handle.notify(Msg::Hang).await.unwrap();

    let deadline = Instant::now() + Duration::from_millis(2_000);
    while !entered.load(Ordering::SeqCst) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(entered.load(Ordering::SeqCst), "actor must have hung");

    let policy = ShutdownPolicy {
        timeout: Duration::from_millis(300),
        per_actor_timeout: Duration::from_millis(100),
    };
    let shutdown_task = tokio::spawn({
        let sys = sys.clone();
        async move { sys.shutdown_with(policy).await }
    });

    // Shutdown flips to `ShuttingDown` synchronously at entry, well before
    // this sleep elapses.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !sys.reactivate(),
        "reactivate must be refused while a shutdown is still in flight"
    );

    tokio::time::timeout(Duration::from_secs(5), shutdown_task)
        .await
        .expect("shutdown must not hang")
        .expect("shutdown task must not panic");

    assert!(
        sys.reactivate(),
        "once shutdown has fully completed the system is Defunct and reactivatable"
    );
}
