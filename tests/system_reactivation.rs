//! Behavioral suite for system reactivation.
//!
//! `ActorSystem::shutdown`/`shutdown_with` move a system through a tri-state
//! phase: `Active` -> `ShuttingDown` (as soon as shutdown is called) ->
//! `Defunct` (once every root has stopped). Both non-`Active` phases reject
//! new registrations with `SpawnError::SystemShuttingDown`, but unlike a
//! one-way flag, a `Defunct` system is not poisoned forever:
//! `ActorSystem::reactivate` returns it to `Active`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, ShutdownPolicy, SpawnError, StopReason,
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

/// Holds its graceful stop in `pre_stop` until the test opens the gate, so a
/// shutdown stays in flight for exactly as long as the test wants.
struct Held {
    gate: Arc<tokio::sync::Notify>,
}

impl Actor for Held {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.gate.notified().await;
        true
    }
}

/// Opens the gate when dropped, also while a failed assertion unwinds.
struct OpensOnDrop(Arc<tokio::sync::Notify>);

impl Drop for OpensOnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reactivate_refused_while_shutdown_is_in_flight() {
    let sys = ActorSystem::create(uname("mid-flight")).unwrap();

    let gate = Arc::new(tokio::sync::Notify::new());
    let _opens = OpensOnDrop(gate.clone());
    Held { gate: gate.clone() }
        .spawn()
        .named("held-root")
        .on_system(&sys)
        .await
        .unwrap();

    // Neither bound can end the shutdown before the gate opens.
    let policy = ShutdownPolicy {
        timeout: Duration::from_secs(60),
        per_actor_timeout: Duration::from_secs(60),
    };
    // The first poll of the call moves the system to `ShuttingDown` and
    // counts the shutdown as in flight; the held root keeps it in flight
    // until the test opens the gate.
    let mut shutdown = Box::pin(sys.shutdown_with(policy));
    let pending =
        std::future::poll_fn(|cx| std::task::Poll::Ready(shutdown.as_mut().poll(cx).is_pending()))
            .await;
    assert!(pending, "a shutdown with a held root cannot end at once");
    assert!(
        !sys.reactivate(),
        "reactivate must be refused while a shutdown is still in flight"
    );

    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(10), shutdown)
        .await
        .expect("shutdown must not hang");

    assert!(
        sys.reactivate(),
        "once shutdown has fully completed the system is Defunct and reactivatable"
    );
}
