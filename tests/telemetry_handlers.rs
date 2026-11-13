#![allow(dead_code)]
//! Attaching and detaching telemetry handlers by id through the public API.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tokio_actors::actor::context::ActorContext;
use tokio_actors::telemetry::{EventKind, KindSet, TelemetryEvent};
use tokio_actors::{Actor, ActorConfig, ActorExt, ActorResult, ActorSystem};

fn system() -> Arc<ActorSystem> {
    ActorSystem::create(format!("telemetry-handlers-{}", uuid::Uuid::new_v4())).unwrap()
}

fn kinds() -> KindSet {
    KindSet::of(&[EventKind::ActorEnded])
}

/// Counts its drop and panics in it.
struct Bomb(Arc<AtomicUsize>);

impl Drop for Bomb {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("dropped");
    }
}

#[test]
fn attach_and_detach_answer_by_id() {
    let sys = system();
    // (operation, id, answer)
    let table = [
        ("detach", "a", false),
        ("attach", "a", true),
        ("attach", "a", false),
        ("attach", "b", true),
        ("detach", "a", true),
        ("detach", "a", false),
        ("attach", "a", true),
        ("detach", "b", true),
        ("detach", "a", true),
    ];
    for (op, id, expected) in table {
        let got = if op == "attach" {
            sys.attach_telemetry(id, kinds(), |_| {})
        } else {
            sys.detach_telemetry(id)
        };
        assert_eq!(got, expected, "{op} {id}");
    }
}

/// An actor that does nothing.
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A flag a handler blocks on until the test opens it.
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    entered: AtomicBool,
}

impl Gate {
    fn block(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        while !*open {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            open = self
                .changed
                .wait_timeout(open, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    fn release(&self) {
        *self.open.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.changed.notify_all();
    }
}

/// Opens its gate when dropped, so a failed assertion never leaves a handler
/// holding the delivery thread.
struct OpensOnDrop(Arc<Gate>);

impl Drop for OpensOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A refused spawn is told to the system it names, and one that names none to
/// the default system.
#[test]
fn a_refused_spawn_is_told_to_its_system_or_else_the_default_system() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let default = ActorSystem::default();
        let named = system();
        let attach = |sys: &ActorSystem| {
            let seen: Arc<Mutex<Vec<String>>> = Arc::default();
            let sink = seen.clone();
            let kinds = KindSet::of(&[EventKind::SpawnRefused]);
            assert!(sys.attach_telemetry("refused", kinds, move |event| {
                if let TelemetryEvent::SpawnRefused { name, .. } = event {
                    sink.lock()
                        .unwrap()
                        .push(name.as_deref().unwrap_or("").into());
                }
            }));
            seen
        };
        let (in_default, in_named) = (attach(&default), attach(&named));
        let zero = ActorConfig::default().with_mailbox_capacity(0);
        assert!(Idle
            .spawn()
            .named("nowhere")
            .with_config(zero.clone())
            .await
            .is_err());
        assert!(Idle
            .spawn()
            .named("somewhere")
            .on_system(&named)
            .with_config(zero)
            .await
            .is_err());
        let deadline = Instant::now() + Duration::from_secs(20);
        while in_default.lock().unwrap().is_empty() || in_named.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "the refusals were not told");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*in_default.lock().unwrap(), ["nowhere"]);
        assert_eq!(*in_named.lock().unwrap(), ["somewhere"]);
    });
}
