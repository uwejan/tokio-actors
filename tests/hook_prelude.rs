//! A hook written as a plain `fn` that returns a future, with code before its
//! `async` block, runs that code inside the catch like the rest of the hook:
//! a panic there is the actor's `Failure(Panic(..))`, `on_stopped` runs, and a
//! `send` caller reads `AskError::Actor(Panic(..))`.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, AskError, ChildEvent, RestartType, SpawnError, StopReason,
    SupervisionAction,
};

mod common;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hook {
    PreStart,
    OnStarted,
    Handle,
    PreStop,
    OnStopped,
}

struct Plain {
    hook: Hook,
    stopped: Arc<AtomicU32>,
    last_reason: Arc<std::sync::Mutex<Option<StopReason>>>,
}

impl Actor for Plain {
    type Message = String;
    type Response = i64;

    fn pre_start(
        &mut self,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        if self.hook == Hook::PreStart {
            panic!("sync pre_start boom");
        }
        async { Ok(()) }
    }

    fn on_started(
        &mut self,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        if self.hook == Hook::OnStarted {
            panic!("sync on_started boom");
        }
        async { Ok(()) }
    }

    fn handle(
        &mut self,
        msg: String,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<i64>> + Send {
        let n: i64 = if self.hook == Hook::Handle {
            msg.parse().expect("sync handle boom")
        } else {
            1
        };
        async move { Ok(n) }
    }

    fn pre_stop(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = bool> + Send {
        if self.hook == Hook::PreStop {
            panic!("sync pre_stop boom");
        }
        async { true }
    }

    fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        self.stopped.fetch_add(1, Ordering::SeqCst);
        *self.last_reason.lock().unwrap() = Some(reason.clone());
        if self.hook == Hook::OnStopped {
            panic!("sync on_stopped boom");
        }
        async { Ok(()) }
    }
}

fn plain(
    hook: Hook,
) -> (
    Plain,
    Arc<AtomicU32>,
    Arc<std::sync::Mutex<Option<StopReason>>>,
) {
    let stopped = Arc::new(AtomicU32::new(0));
    let last = Arc::new(std::sync::Mutex::new(None));
    (
        Plain {
            hook,
            stopped: stopped.clone(),
            last_reason: last.clone(),
        },
        stopped,
        last,
    )
}

fn panic_text(reason: &StopReason) -> String {
    match reason {
        StopReason::Failure(ActorError::Panic(text)) => text.clone(),
        other => format!("not a panic failure: {other:?}"),
    }
}

#[test]
fn handle_sync_prelude_panic_is_caught() {
    common::run(2, async {
        let (actor, stopped, last) = plain(Hook::Handle);
        let h = actor.spawn().await.unwrap();
        let got = h.send("abc".to_string()).await;
        eprintln!("a prelude panic in handle: send -> {got:?}");
        let reason = tokio::time::timeout(Duration::from_secs(5), h.wait_stopped())
            .await
            .expect("the actor ends");
        eprintln!(
            "a prelude panic in handle: reason = {reason:?}, on_stopped runs = {}, on_stopped saw = {:?}",
            stopped.load(Ordering::SeqCst),
            last.lock().unwrap()
        );
        match got {
            Err(AskError::Actor(ActorError::Panic(text))) => {
                assert!(text.contains("sync handle boom"), "{text}")
            }
            other => panic!("the send caller must see Actor(Panic(..)), got {other:?}"),
        }
        assert!(panic_text(&reason).contains("sync handle boom"));
        assert_eq!(stopped.load(Ordering::SeqCst), 1, "on_stopped runs once");
    });
}

#[test]
fn pre_start_sync_prelude_panic_is_caught() {
    common::run(2, async {
        let (actor, stopped, _last) = plain(Hook::PreStart);
        let got = actor.spawn().await;
        eprintln!(
            "a prelude panic in pre_start: spawn -> {:?}",
            got.as_ref().err()
        );
        match got {
            Err(SpawnError::Init(err)) => match *err {
                ActorError::Panic(text) => {
                    assert!(text.contains("sync pre_start boom"), "{text}")
                }
                other => panic!("expected a panic, got {other:?}"),
            },
            other => panic!(
                "expected Init(Panic(sync pre_start boom)), got {:?}",
                other.err()
            ),
        }
        assert_eq!(
            stopped.load(Ordering::SeqCst),
            0,
            "no on_stopped after a failed init"
        );
    });
}

#[test]
fn on_started_sync_prelude_panic_is_caught() {
    common::run(2, async {
        let (actor, _stopped, _last) = plain(Hook::OnStarted);
        let got = actor.spawn().await;
        eprintln!(
            "a prelude panic in on_started: spawn -> {:?}",
            got.as_ref().err()
        );
        match got {
            Err(SpawnError::Init(err)) => match *err {
                ActorError::Panic(text) => {
                    assert!(text.contains("sync on_started boom"), "{text}")
                }
                other => panic!("expected a panic, got {other:?}"),
            },
            other => panic!(
                "expected Init(Panic(sync on_started boom)), got {:?}",
                other.err()
            ),
        }
    });
}

#[test]
fn pre_stop_sync_prelude_panic_is_caught() {
    common::run(2, async {
        let (actor, stopped, last) = plain(Hook::PreStop);
        let h = actor.spawn().await.unwrap();
        h.stop(StopReason::Graceful).await.unwrap();
        let reason = tokio::time::timeout(Duration::from_secs(5), h.wait_stopped())
            .await
            .expect("the actor ends");
        eprintln!(
            "a prelude panic in pre_stop: reason = {reason:?}, on_stopped runs = {}, saw = {:?}",
            stopped.load(Ordering::SeqCst),
            last.lock().unwrap()
        );
        assert!(
            panic_text(&reason).contains("sync pre_stop boom"),
            "{reason:?}"
        );
        assert_eq!(stopped.load(Ordering::SeqCst), 1, "on_stopped runs once");
    });
}

#[test]
fn on_stopped_sync_prelude_panic_is_caught() {
    common::run(2, async {
        let (actor, stopped, _last) = plain(Hook::OnStopped);
        let h = actor.spawn().await.unwrap();
        h.stop(StopReason::Graceful).await.unwrap();
        let reason = tokio::time::timeout(Duration::from_secs(5), h.wait_stopped())
            .await
            .expect("the actor ends");
        eprintln!(
            "a prelude panic in on_stopped: reason = {reason:?}, on_stopped runs = {}",
            stopped.load(Ordering::SeqCst)
        );
        assert!(
            panic_text(&reason).contains("sync on_stopped boom"),
            "{reason:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// on_child_stopped, delivered from the supervisor's loop
// ---------------------------------------------------------------------------

struct Crasher;

impl Actor for Crasher {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("child crashed"))
    }
}

struct Watcher {
    stopped: Arc<AtomicU32>,
    last_reason: Arc<std::sync::Mutex<Option<StopReason>>>,
}

impl Actor for Watcher {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let child = ctx
            .spawn_child(|| Crasher)
            .restart_type(RestartType::Temporary)
            .await?;
        child.notify(()).await.ok();
        Ok(())
    }

    fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        if event.action != SupervisionAction::SupervisorStopping {
            panic!("sync on_child_stopped boom");
        }
        async { Ok(()) }
    }

    fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        self.stopped.fetch_add(1, Ordering::SeqCst);
        *self.last_reason.lock().unwrap() = Some(reason.clone());
        async { Ok(()) }
    }
}

#[test]
fn on_child_stopped_sync_prelude_panic_is_caught() {
    common::run(2, async {
        let stopped = Arc::new(AtomicU32::new(0));
        let last = Arc::new(std::sync::Mutex::new(None));
        let sup = Watcher {
            stopped: stopped.clone(),
            last_reason: last.clone(),
        }
        .spawn()
        .supervisor()
        .await
        .unwrap();
        sup.notify(()).await.unwrap();
        let reason = tokio::time::timeout(Duration::from_secs(10), sup.wait_stopped())
            .await
            .expect("the supervisor ends");
        eprintln!(
            "a prelude panic in on_child_stopped: reason = {reason:?}, on_stopped runs = {}",
            stopped.load(Ordering::SeqCst)
        );
        assert!(
            panic_text(&reason).contains("sync on_child_stopped boom"),
            "{reason:?}"
        );
        assert_eq!(stopped.load(Ordering::SeqCst), 1, "on_stopped runs once");
        assert_eq!(*last.lock().unwrap(), Some(reason));
    });
}
