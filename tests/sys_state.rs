#![allow(dead_code)]
//! State access through `Sys::inspect` and `Sys::replace_state`, and the
//! `state` of `get_status`: closed unless the spawn opts in, never reachable
//! for a sensitive actor, user closures run on the actor's task as a turn of
//! their own inside a catch, and a request whose caller left never runs.
//!
//! Each case runs on a current-thread and on a multi-thread runtime.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    sys::SysError,
    ActorError, ActorResult, ActorStatus, ActorSystem, StopReason, SystemConfig,
};

/// Bounds how long a hang takes to report; a slow host must not read as one.
const BOUND: Duration = Duration::from_secs(30);

/// Expands one case into a current-thread test and a multi-thread test.
macro_rules! both_flavors {
    ($name:ident, $body:ident) => {
        mod $name {
            #[tokio::test]
            async fn current_thread() {
                super::$body().await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn multi_thread() {
                super::$body().await;
            }
        }
    };
}

fn system(base: &str) -> Arc<ActorSystem> {
    ActorSystem::create(format!("sys-state-{base}-{}", uuid::Uuid::new_v4())).unwrap()
}

/// What an actor's `format_status` does.
#[derive(Clone, Copy, PartialEq)]
enum Format {
    Default,
    Text,
    Panic,
}

enum Msg {
    Add(u64),
    /// Signals `.0` when the handler begins, then waits for `.1`.
    Park(oneshot::Sender<()>, oneshot::Receiver<()>),
}

/// An actor holding a number. `S` makes it sensitive by its own constant.
struct Store<const S: bool> {
    value: u64,
    format: Format,
}

impl<const S: bool> Store<S> {
    fn new(value: u64) -> Self {
        Self {
            value,
            format: Format::Default,
        }
    }

    fn formatting(value: u64, format: Format) -> Self {
        Self { value, format }
    }
}

impl<const S: bool> Actor for Store<S> {
    type Message = Msg;
    type Response = u64;
    const SENSITIVE: bool = S;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
        match msg {
            Msg::Add(n) => self.value += n,
            Msg::Park(entered, release) => {
                let _ = entered.send(());
                let _ = release.await;
            }
        }
        Ok(self.value)
    }

    fn format_status(&self) -> Option<String> {
        match self.format {
            Format::Default => None,
            Format::Text => Some(format!("value={}", self.value)),
            Format::Panic => panic!("format failed"),
        }
    }
}

/// Parks the actor's handler and returns the sender that releases it, once
/// the handler is inside.
async fn park<const S: bool>(handle: &tokio_actors::ActorHandle<Store<S>>) -> oneshot::Sender<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    handle
        .notify(Msg::Park(entered_tx, release_rx))
        .await
        .unwrap();
    timeout(BOUND, entered_rx)
        .await
        .expect("the handler is entered")
        .unwrap();
    release_tx
}

/// A value whose drop panics, counting the drops.
struct PanicsOnDrop(Arc<AtomicUsize>);

impl Drop for PanicsOnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("the captured value panicked in drop");
    }
}

// ---------------------------------------------------------------------------
// Who may reach the state
// ---------------------------------------------------------------------------

/// Closed unless the spawn opts in; `sensitive`, by builder or by constant,
/// wins over the opt-in; a handle fetched by id reaches an opted-in actor.
async fn access_follows_the_spawn() {
    let sys = system("access");
    let closed = Store::<false>::new(1)
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();
    let open = Store::<false>::new(7)
        .spawn()
        .allow_state_access()
        .on_system(&sys)
        .await
        .unwrap();
    let built_sensitive = Store::<false>::new(3)
        .spawn()
        .allow_state_access()
        .sensitive()
        .on_system(&sys)
        .await
        .unwrap();
    let const_sensitive = Store::<true>::new(4)
        .spawn()
        .allow_state_access()
        .on_system(&sys)
        .await
        .unwrap();

    let ran = Arc::new(AtomicBool::new(false));
    let touch = |ran: &Arc<AtomicBool>| {
        let ran = ran.clone();
        move |_: &Store<false>| ran.store(true, Ordering::SeqCst)
    };
    assert_eq!(
        closed.sys().inspect(touch(&ran)).await,
        Err(SysError::NotAllowed)
    );
    assert_eq!(
        closed.sys().replace_state(|s| s.value = 9).await,
        Err(SysError::NotAllowed)
    );
    assert_eq!(
        built_sensitive.sys().inspect(touch(&ran)).await,
        Err(SysError::Sensitive)
    );
    assert_eq!(
        built_sensitive.sys().replace_state(|s| s.value = 9).await,
        Err(SysError::Sensitive)
    );
    assert_eq!(
        const_sensitive.sys().inspect(|s| s.value).await,
        Err(SysError::Sensitive)
    );
    assert_eq!(
        const_sensitive.sys().replace_state(|s| s.value = 9).await,
        Err(SysError::Sensitive)
    );
    assert!(!ran.load(Ordering::SeqCst), "a refused closure never runs");
    assert_eq!(
        closed.send(Msg::Add(0)).await.unwrap(),
        1,
        "a refused change left the value alone"
    );

    assert_eq!(open.sys().inspect(|s| s.value).await, Ok(7));
    let by_id = sys.get_by_id::<Store<false>>(open.id()).unwrap();
    assert_eq!(by_id.sys().inspect(|s| s.value * 2).await, Ok(14));
    assert_eq!(by_id.sys().replace_state(|s| s.value = 41).await, Ok(()));
    assert_eq!(
        open.sys().inspect(|s| s.value).await,
        Ok(41),
        "a change made through one handle shows through another"
    );
}

both_flavors!(access, access_follows_the_spawn);

// ---------------------------------------------------------------------------
// Panics in the closures
// ---------------------------------------------------------------------------

/// A reading closure that panics is answered and the actor goes on; a
/// changing closure that panics is answered and the actor ends with the
/// panic as its failure.
async fn a_panicking_closure_is_answered_and_a_change_ends_the_actor() {
    let sys = system("panics");
    let reader = Store::<false>::new(5)
        .spawn()
        .allow_state_access()
        .on_system(&sys)
        .await
        .unwrap();
    let answer = reader
        .sys()
        .inspect(|_| -> u64 { panic!("read failed") })
        .await;
    assert_eq!(answer, Err(SysError::Callback("read failed".to_string())));
    assert_eq!(
        reader.send(Msg::Add(1)).await.unwrap(),
        6,
        "the actor handles the next message"
    );
    assert_eq!(reader.status(), ActorStatus::Running);

    let writer = Store::<false>::new(5)
        .spawn()
        .allow_state_access()
        .on_system(&sys)
        .await
        .unwrap();
    let answer = writer
        .sys()
        .replace_state(|s| {
            s.value = 99;
            panic!("write failed")
        })
        .await;
    assert_eq!(answer, Err(SysError::Callback("write failed".to_string())));
    let reason = timeout(BOUND, writer.wait_stopped())
        .await
        .expect("the actor ends");
    assert_eq!(
        reason,
        StopReason::Failure(ActorError::Panic("write failed".to_string()))
    );
}

both_flavors!(
    panics,
    a_panicking_closure_is_answered_and_a_change_ends_the_actor
);

// ---------------------------------------------------------------------------
// The closure is a turn
// ---------------------------------------------------------------------------

/// A closure that holds the actor's task shows in `stuck_actors`, which only
/// a turn the watchdog brackets does. The closure waits until a thread of the
/// test has seen it there.
async fn a_closure_is_a_turn_the_watchdog_sees() {
    let sys = ActorSystem::create_with(
        format!("sys-state-turn-{}", uuid::Uuid::new_v4()),
        SystemConfig::default().with_turn_warning_threshold(Duration::from_millis(20)),
    )
    .unwrap();
    let handle = Store::<false>::new(1)
        .spawn()
        .allow_state_access()
        .on_system(&sys)
        .await
        .unwrap();

    let seen = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (sys, id, seen, done) = (sys.clone(), handle.id().clone(), seen.clone(), done.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                if sys.stuck_actors().iter().any(|(stuck, _)| *stuck == id) {
                    seen.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let hold = {
        let seen = seen.clone();
        move || {
            let until = std::time::Instant::now() + BOUND;
            while !seen.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    };

    let read = {
        let hold = hold.clone();
        handle.sys().inspect(move |_| hold()).await
    };
    let read_seen = seen.swap(false, Ordering::SeqCst);
    let write = handle.sys().replace_state(move |_| hold()).await;
    let write_seen = seen.load(Ordering::SeqCst);
    done.store(true, Ordering::SeqCst);
    sampler.join().unwrap();

    assert_eq!(read, Ok(()));
    assert_eq!(write, Ok(()));
    assert!(read_seen, "an inspect closure was a turn in flight");
    assert!(write_seen, "a replace_state closure was a turn in flight");
}

both_flavors!(turn, a_closure_is_a_turn_the_watchdog_sees);

// ---------------------------------------------------------------------------
// A request whose caller left, or whose actor ended
// ---------------------------------------------------------------------------

/// A request whose caller timed out is dropped unrun, and its captured value
/// is dropped inside a catch: the actor goes on.
async fn a_request_whose_caller_left_is_dropped_unrun_inside_a_catch() {
    let sys = system("gone");
    let handle = Store::<false>::new(1)
        .spawn()
        .allow_state_access()
        .on_system(&sys)
        .await
        .unwrap();
    let release = park(&handle).await;

    let ran = Arc::new(AtomicBool::new(false));
    let drops = Arc::new(AtomicUsize::new(0));
    let guard = PanicsOnDrop(drops.clone());
    let answer = handle
        .sys()
        .timeout(Duration::from_millis(30))
        .replace_state({
            let ran = ran.clone();
            move |s| {
                let _guard = &guard;
                ran.store(true, Ordering::SeqCst);
                s.value = 1000;
            }
        })
        .await;
    assert_eq!(answer, Err(SysError::Timeout));

    release.send(()).unwrap();
    assert_eq!(
        timeout(BOUND, handle.send(Msg::Add(1)))
            .await
            .expect("the actor serves the next message")
            .expect("the actor is alive"),
        2,
        "the abandoned change was not applied and the actor went on"
    );
    assert!(!ran.load(Ordering::SeqCst), "the closure never ran");
    assert_eq!(drops.load(Ordering::SeqCst), 1, "its capture was dropped");
    assert_eq!(handle.status(), ActorStatus::Running);

    // A change that has begun is not dropped when its caller's bound passes:
    // the caller is told `Timeout`, the function runs to its end, and the
    // value is the replaced one. The function blocks its thread, so the
    // step needs a worker thread besides the one it holds.
    if tokio::runtime::Handle::current().runtime_flavor()
        != tokio::runtime::RuntimeFlavor::MultiThread
    {
        return;
    }
    let handle = Store::<false>::new(1)
        .spawn()
        .allow_state_access()
        .on_system(&sys)
        .await
        .unwrap();
    let (begun_tx, begun_rx) = oneshot::channel();
    let (open_tx, open_rx) = std::sync::mpsc::channel::<()>();
    let call = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .sys()
                .timeout(Duration::from_millis(300))
                .replace_state(move |s| {
                    let _ = begun_tx.send(());
                    // Parked on a blocking gate; a hang ends at the bound.
                    open_rx.recv_timeout(BOUND).expect("the gate opens");
                    s.value = 1000;
                })
                .await
        })
    };
    timeout(BOUND, begun_rx)
        .await
        .expect("the function begins")
        .unwrap();
    let answer = timeout(BOUND, call)
        .await
        .expect("the call is answered")
        .unwrap();
    assert_eq!(answer, Err(SysError::Timeout));
    open_tx.send(()).unwrap();
    assert_eq!(
        timeout(BOUND, handle.sys().inspect(|s| s.value))
            .await
            .expect("the actor serves the next request"),
        Ok(1000),
        "a begun change that answered `Timeout` was applied"
    );
    assert_eq!(handle.status(), ActorStatus::Running);
}

both_flavors!(
    gone,
    a_request_whose_caller_left_is_dropped_unrun_inside_a_catch
);

/// A request queued behind a running handler, when the actor is stopped:
/// its captured value panics in drop, the actor's reason stays the one that
/// stopped it, and the caller reads that reason. The handler of the `Kill`
/// row never returns; the one of the `Graceful` row is released after the
/// stop is raised.
async fn a_queued_request_dropped_at_the_actors_end_leaves_the_reason() {
    for (reason, release_handler) in [(StopReason::Kill, false), (StopReason::Graceful, true)] {
        let sys = system("queued");
        let handle = Store::<false>::new(1)
            .spawn()
            .allow_state_access()
            .kill_grace(Duration::from_millis(50))
            .on_system(&sys)
            .await
            .unwrap();
        let release = park(&handle).await;

        let drops = Arc::new(AtomicUsize::new(0));
        let guard = PanicsOnDrop(drops.clone());
        let sys_calls = handle.sys();
        let mut call = Box::pin(sys_calls.inspect(move |s| {
            let _guard = &guard;
            s.value
        }));
        // One poll queues the request; the answer cannot come while the
        // handler is parked.
        let queued =
            std::future::poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx).is_pending())).await;
        assert!(queued, "the request is queued behind the parked handler");

        handle.stop(reason.clone()).await.unwrap();
        let _held = if release_handler {
            release.send(()).unwrap();
            None
        } else {
            Some(release)
        };
        let ended = timeout(BOUND, handle.wait_stopped())
            .await
            .expect("the actor ends");
        assert_eq!(ended, reason);
        assert_eq!(
            timeout(BOUND, call).await.expect("the call is answered"),
            Err(SysError::ActorEnded(reason.clone())),
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1, "the capture was dropped");
    }
}

both_flavors!(
    queued,
    a_queued_request_dropped_at_the_actors_end_leaves_the_reason
);

// ---------------------------------------------------------------------------
// format_status
// ---------------------------------------------------------------------------

/// `get_status().state` is the actor's `format_status`: nothing by default
/// and for a sensitive actor, a placeholder when it panics, and the actor
/// lives on.
async fn the_status_reply_shows_format_status() {
    let sys = system("format");
    let shown = Store::<false>::formatting(3, Format::Text)
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();
    let silent = Store::<false>::new(3)
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();
    let built_sensitive = Store::<false>::formatting(3, Format::Text)
        .spawn()
        .sensitive()
        .on_system(&sys)
        .await
        .unwrap();
    let const_sensitive = Store::<true>::formatting(3, Format::Text)
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();
    let panicking = Store::<false>::formatting(3, Format::Panic)
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();

    let state = |info: tokio_actors::ActorStatusInfo| info.state;
    assert_eq!(
        state(shown.get_status().await.unwrap()),
        Some("value=3".to_string())
    );
    assert_eq!(state(silent.get_status().await.unwrap()), None);
    assert_eq!(state(built_sensitive.get_status().await.unwrap()), None);
    assert_eq!(state(const_sensitive.get_status().await.unwrap()), None);
    assert_eq!(
        state(panicking.get_status().await.unwrap()),
        Some("format_status panicked".to_string())
    );
    assert_eq!(
        panicking.send(Msg::Add(1)).await.unwrap(),
        4,
        "a format_status that panicked did not end the actor"
    );
    assert_eq!(
        state(panicking.get_status().await.unwrap()),
        Some("format_status panicked".to_string()),
        "and the next status reply is answered the same way"
    );
}

both_flavors!(format, the_status_reply_shows_format_status);
