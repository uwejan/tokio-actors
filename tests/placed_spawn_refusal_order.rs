//! The order of the refusals of a placed spawn: a refusal that is not one of
//! the three early ones (not a supervisor, a supervisor that is stopping, a
//! child name already held) is answered after the check of the placed
//! runtime, so on a runtime without the time driver the spawn panics first,
//! and on a runtime with the driver the same spawn is refused with its own
//! error.

mod common;

use std::future::{Future, IntoFuture};
use std::panic::{self, AssertUnwindSafe};
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::runtime::{Builder, Handle, Runtime};
use tokio_actors::{
    actor::context::ActorContext, Actor, ActorConfig, ActorExt, ActorHandle, ActorResult,
    ActorSystem, SpawnError, StopReason, SupervisionConfig,
};

/// The longest a wait for an outcome may take. It only limits how long a real
/// hang takes to report.
const HANG_BOUND: Duration = Duration::from_secs(60);

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

fn unique(base: &str) -> String {
    format!(
        "placed-refusals-{base}-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

fn with_driver() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

fn without_driver() -> Runtime {
    Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap()
}

fn end_runtime(runtime: Runtime) {
    std::thread::spawn(move || drop(runtime)).join().unwrap();
}

struct Worker;

impl Actor for Worker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// What a spawn is told to do.
#[derive(Clone)]
struct Spec {
    name: String,
    runtime: Handle,
    zero_capacity: bool,
    /// Counts every call of the factory.
    built: Arc<AtomicUsize>,
}

impl Spec {
    fn new(base: &str, runtime: &Handle) -> Self {
        Self {
            name: unique(base),
            runtime: runtime.clone(),
            zero_capacity: false,
            built: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// The outcome of one poll of a spawn.
struct Attempt {
    /// The text of the panic the poll raised.
    panic: Option<String>,
    /// The answer, when the spawn finished in that poll.
    answer: Option<Result<(), SpawnError>>,
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => payload
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .unwrap_or_default(),
    }
}

struct Supervisor;

impl Actor for Supervisor {
    type Message = Spec;
    type Response = Attempt;

    /// Polls the spawn of `spec` once, with a waker that does nothing, and
    /// catches a panic.
    async fn handle(&mut self, spec: Spec, ctx: &mut ActorContext<Self>) -> ActorResult<Attempt> {
        let built = spec.built.clone();
        let mut builder = ctx
            .spawn_child(move || {
                built.fetch_add(1, Ordering::SeqCst);
                Worker
            })
            .named(spec.name.clone())
            .on_runtime(&spec.runtime);
        if spec.zero_capacity {
            builder = builder.with_config(ActorConfig::default().with_mailbox_capacity(0));
        }
        let caught = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut future = builder.into_future();
            let mut cx = Context::from_waker(Waker::noop());
            match pin!(&mut future).poll(&mut cx) {
                Poll::Ready(answer) => Some(answer.map(|_: ActorHandle<Worker>| ())),
                Poll::Pending => None,
            }
        }));
        Ok(match caught {
            Ok(answer) => Attempt {
                panic: None,
                answer,
            },
            Err(payload) => Attempt {
                panic: Some(panic_text(payload)),
                answer: None,
            },
        })
    }
}

async fn supervisor() -> ActorHandle<Supervisor> {
    Supervisor
        .spawn()
        .named(unique("supervisor"))
        .with_supervision(SupervisionConfig::one_for_one())
        .await
        .unwrap()
}

async fn attempt_via(sup: &ActorHandle<Supervisor>, spec: &Spec) -> Attempt {
    sup.send(spec.clone()).await.unwrap()
}

async fn finish(sup: ActorHandle<Supervisor>) {
    let _ = sup.stop(StopReason::Graceful).await;
    tokio::time::timeout(HANG_BOUND, sup.wait_stopped())
        .await
        .expect("the supervisor ends");
}

#[test]
fn a_zero_mailbox_capacity_is_refused_after_the_check_of_the_placed_runtime() {
    let bare = without_driver();
    let good = with_driver();
    let bare_handle = bare.handle().clone();
    let good_handle = good.handle().clone();
    common::run(2, async move {
        let sup = supervisor().await;

        // Without the driver the check panics first, before the factory runs.
        let mut spec = Spec::new("zero-bare", &bare_handle);
        spec.zero_capacity = true;
        let attempted = attempt_via(&sup, &spec).await;
        let text = attempted
            .panic
            .expect("the check of the runtime comes before the capacity refusal");
        assert!(text.contains("enable_time"), "{text}");
        assert!(text.contains("ChildSpawnBuilder::on_runtime"), "{text}");
        assert_eq!(attempted.answer, None);
        assert_eq!(spec.built.load(Ordering::SeqCst), 0, "no factory call");

        // With the driver the same spawn passes the check and is refused for
        // its capacity.
        let mut spec = Spec::new("zero-good", &good_handle);
        spec.zero_capacity = true;
        let attempted = attempt_via(&sup, &spec).await;
        assert_eq!(attempted.panic, None, "the check passes");
        assert_eq!(attempted.answer, Some(Err(SpawnError::ZeroMailboxCapacity)));
        finish(sup).await;
    });
    end_runtime(bare);
    end_runtime(good);
}

#[test]
fn a_name_taken_in_the_system_is_refused_after_the_check_of_the_placed_runtime() {
    let bare = without_driver();
    let good = with_driver();
    let bare_handle = bare.handle().clone();
    let good_handle = good.handle().clone();
    common::run(2, async move {
        let sup = supervisor().await;
        // A plain actor outside the supervisor holds the name, so the
        // supervisor's own roster does not know it.
        let held_name = unique("held");
        let held = Worker.spawn().named(held_name.clone()).await.unwrap();

        // Without the driver the check panics first, before the factory runs.
        let spec = Spec {
            name: held_name.clone(),
            ..Spec::new("held-bare", &bare_handle)
        };
        let attempted = attempt_via(&sup, &spec).await;
        let text = attempted
            .panic
            .expect("the check of the runtime comes before the name refusal");
        assert!(text.contains("enable_time"), "{text}");
        assert!(text.contains("ChildSpawnBuilder::on_runtime"), "{text}");
        assert_eq!(attempted.answer, None);
        assert_eq!(spec.built.load(Ordering::SeqCst), 0, "no factory call");

        // With the driver the same spawn passes the check and is refused for
        // its name.
        let spec = Spec {
            name: held_name.clone(),
            ..Spec::new("held-good", &good_handle)
        };
        let attempted = attempt_via(&sup, &spec).await;
        assert_eq!(attempted.panic, None, "the check passes");
        assert!(
            matches!(attempted.answer, Some(Err(SpawnError::NameTaken { .. }))),
            "the name is taken in the system"
        );
        assert!(
            ActorSystem::default().get::<Worker>(&held_name).is_some(),
            "the holder is untouched"
        );
        let _ = held.stop(StopReason::Graceful).await;
        finish(sup).await;
    });
    end_runtime(bare);
    end_runtime(good);
}
