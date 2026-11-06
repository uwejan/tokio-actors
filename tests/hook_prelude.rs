//! A hook written as a plain `fn` that returns a future, with code before its
//! `async` block, runs that code inside the catch like the rest of the hook:
//! a panic there is the actor's `Failure(Panic(..))`, `on_stopped` runs, and a
//! `send` caller reads `AskError::Actor(Panic(..))`.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, SpawnError, StopReason,
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

// ---------------------------------------------------------------------------
// on_child_stopped, delivered from the supervisor's loop
// ---------------------------------------------------------------------------
