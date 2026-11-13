#![allow(dead_code)]
//! `ActorContext::stop` ends the actor the way the handle route does, never
//! weakens a request already on the lane, refuses `Kill`, and holds the reply
//! of the same turn to the seal of the stop lane: the caller reads it after
//! `on_stopped` has returned and the name is free, as the caller of an
//! Erlang/OTP `gen_server` reads `{stop, Reason, Reply, State}`.
//!
//! Nothing is ordered by a sleep: the actor holds its hooks and handlers at
//! gates the tests open. The windows only give an early answer time to
//! arrive, and the bounds only detect a hang.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, Semaphore};
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorSystem, AskError, StopError,
    StopReason,
};

mod common;

/// The longest a wait may take. It only limits how long a real hang takes to
/// report.
const BOUND: Duration = Duration::from_secs(30);

/// The longest a call that must return at once may take.
const PROMPT: Duration = Duration::from_secs(30);

/// How long a call that must still be waiting is watched.
const WINDOW: Duration = Duration::from_millis(200);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("ctx-stop-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

fn shutdown() -> StopReason {
    StopReason::Shutdown("done".into())
}

fn failure() -> StopReason {
    StopReason::Failure(ActorError::user("f"))
}

/// What a test holds and watches of an actor.
struct Hook {
    /// Set when `on_stopped` begins.
    entered: Notify,
    /// `on_stopped` waits for a permit.
    gate: Semaphore,
    /// Set when `on_stopped` has returned.
    returned: AtomicBool,
    /// Set when a handler that holds itself begins.
    in_handler: Notify,
    /// A handler that holds itself waits for a permit.
    handler_gate: Semaphore,
    /// Every reason `pre_stop` was asked about.
    asked: Mutex<Vec<StopReason>>,
    /// The reason `on_stopped` received.
    stopped_with: Mutex<Option<StopReason>>,
    /// What `ctx.stop` answered inside `on_stopped`.
    late_stop: Mutex<Option<Result<(), StopError>>>,
    /// The `Mark` messages handled so far.
    marks: AtomicUsize,
    /// The `Mark` messages handled when `on_stopped` began.
    marks_at_stop: AtomicUsize,
    /// `on_stopped` has begun.
    ran: AtomicBool,
    /// `pre_stop` stops the actor through the context the first time it is
    /// asked.
    pre_stop_stops: AtomicBool,
}

impl Hook {
    fn new() -> Arc<Hook> {
        Arc::new(Hook {
            entered: Notify::new(),
            gate: Semaphore::new(0),
            returned: AtomicBool::new(false),
            in_handler: Notify::new(),
            handler_gate: Semaphore::new(0),
            asked: Mutex::new(Vec::new()),
            stopped_with: Mutex::new(None),
            late_stop: Mutex::new(None),
            marks: AtomicUsize::new(0),
            marks_at_stop: AtomicUsize::new(usize::MAX),
            ran: AtomicBool::new(false),
            pre_stop_stops: AtomicBool::new(false),
        })
    }

    /// A hook whose `on_stopped` never waits.
    fn open() -> Arc<Hook> {
        let hook = Hook::new();
        hook.gate.add_permits(1 << 20);
        hook
    }

    async fn entered(&self) {
        timeout(BOUND, self.entered.notified())
            .await
            .expect("on_stopped begins");
    }

    async fn in_handler(&self) {
        timeout(BOUND, self.in_handler.notified())
            .await
            .expect("the handler begins");
    }

    fn release(&self) {
        self.gate.add_permits(1);
    }

    fn release_handler(&self) {
        self.handler_gate.add_permits(1);
    }
}

#[derive(Clone, Debug)]
enum Msg {
    /// Stop through the context with the reason, then answer 42.
    ReplyAndStop(StopReason),
    /// Stop through the handle with the reason, then answer 42.
    StopViaHandle(StopReason),
    /// Park at the handler gate, then stop through the context and answer 42.
    HoldThenStop(StopReason),
    /// Stop through the context at once, then park at the handler gate and
    /// answer 42.
    StopThenHold(StopReason),
    /// Try to kill through the context; answers 1 when it was refused.
    TryKill,
    Mark,
    Ping,
}

struct Stopper {
    hook: Arc<Hook>,
    /// Vetoes every request `pre_stop` is asked about.
    veto: bool,
}

impl Actor for Stopper {
    type Message = Msg;
    type Response = u32;

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        match msg {
            Msg::ReplyAndStop(reason) => {
                ctx.stop(reason).expect("the message loop is running");
                Ok(42)
            }
            Msg::StopViaHandle(reason) => {
                ctx.self_handle()
                    .stop(reason)
                    .await
                    .expect("the message loop is running");
                Ok(42)
            }
            Msg::HoldThenStop(reason) => {
                self.hook.in_handler.notify_one();
                self.hook
                    .handler_gate
                    .acquire()
                    .await
                    .expect("the gate stays open")
                    .forget();
                ctx.stop(reason).expect("the message loop is running");
                Ok(42)
            }
            Msg::StopThenHold(reason) => {
                ctx.stop(reason).expect("the message loop is running");
                self.hook.in_handler.notify_one();
                self.hook
                    .handler_gate
                    .acquire()
                    .await
                    .expect("the gate stays open")
                    .forget();
                Ok(42)
            }
            Msg::TryKill => Ok(match ctx.stop(StopReason::Kill) {
                Err(StopError::KillRefused) => 1,
                _ => 0,
            }),
            Msg::Mark => {
                self.hook.marks.fetch_add(1, Ordering::SeqCst);
                Ok(0)
            }
            Msg::Ping => Ok(7),
        }
    }

    async fn pre_stop(&mut self, reason: &StopReason, ctx: &mut ActorContext<Self>) -> bool {
        let first = {
            let mut asked = self.hook.asked.lock().unwrap();
            asked.push(reason.clone());
            asked.len() == 1
        };
        if first && self.hook.pre_stop_stops.load(Ordering::SeqCst) {
            let _ = ctx.stop(StopReason::Graceful);
        }
        !self.veto
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.hook
            .marks_at_stop
            .store(self.hook.marks.load(Ordering::SeqCst), Ordering::SeqCst);
        *self.hook.stopped_with.lock().unwrap() = Some(reason.clone());
        *self.hook.late_stop.lock().unwrap() = Some(ctx.stop(shutdown()));
        self.hook.ran.store(true, Ordering::SeqCst);
        self.hook.entered.notify_one();
        self.hook
            .gate
            .acquire()
            .await
            .expect("the gate stays open")
            .forget();
        self.hook.returned.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// An actor on its own system and the hook the test holds.
struct Rig {
    sys: Arc<ActorSystem>,
    name: String,
    hook: Arc<Hook>,
    handle: ActorHandle<Stopper>,
}

impl Rig {
    async fn start(hook: Arc<Hook>, veto: bool, priority: bool) -> Rig {
        let sys = ActorSystem::create(uname("sys")).expect("a fresh system");
        let name = uname("actor");
        let mut builder = Stopper {
            hook: hook.clone(),
            veto,
        }
        .spawn()
        .named(name.clone())
        .on_system(&sys);
        if priority {
            builder = builder.with_config(ActorConfig::default().with_priority_lane(8));
        }
        let handle = builder.await.expect("the actor starts");
        Rig {
            sys,
            name,
            hook,
            handle,
        }
    }

    async fn ended(&self) -> StopReason {
        timeout(BOUND, self.handle.wait_stopped())
            .await
            .expect("the actor ends")
    }
}

/// How a request is made.
#[derive(Clone, Copy, Debug)]
enum Form {
    Send,
    SendPriority,
}

impl Form {
    async fn call(self, handle: &ActorHandle<Stopper>, msg: Msg) -> Result<u32, AskError> {
        match self {
            Form::Send => handle.send(msg).await,
            Form::SendPriority => handle.send_priority(msg).await,
        }
    }
}

/// What a caller saw the moment its call returned.
#[derive(Debug)]
struct Seen {
    answer: Result<u32, AskError>,
    /// The held `on_stopped` had returned.
    hook_returned: bool,
    /// The actor's name was still registered.
    name_registered: bool,
}

fn ask(rig: &Rig, form: Form, msg: Msg) -> tokio::task::JoinHandle<Seen> {
    let (sys, name, hook, handle) = (
        rig.sys.clone(),
        rig.name.clone(),
        rig.hook.clone(),
        rig.handle.clone(),
    );
    tokio::spawn(async move {
        let answer = form.call(&handle, msg).await;
        Seen {
            answer,
            hook_returned: hook.returned.load(Ordering::SeqCst),
            name_registered: sys.get::<Stopper>(&name).is_some(),
        }
    })
}

// ---------------------------------------------------------------------------
// The two routes agree
// ---------------------------------------------------------------------------

/// What one stop looked like from the actor's hooks and from outside.
#[derive(Debug, PartialEq)]
struct Observed {
    asked: Vec<StopReason>,
    stopped_with: Option<StopReason>,
    exit: StopReason,
    answer: Option<u32>,
}

async fn observe(reason: StopReason, through_context: bool) -> Observed {
    let rig = Rig::start(Hook::open(), false, false).await;
    let msg = if through_context {
        Msg::ReplyAndStop(reason)
    } else {
        Msg::StopViaHandle(reason)
    };
    let answer = timeout(BOUND, rig.handle.send(msg))
        .await
        .expect("the call returns")
        .ok();
    let exit = rig.ended().await;
    let asked = rig.hook.asked.lock().unwrap().clone();
    let stopped_with = rig.hook.stopped_with.lock().unwrap().clone();
    Observed {
        asked,
        stopped_with,
        exit,
        answer,
    }
}

// ---------------------------------------------------------------------------
// The reply goes out at the seal
// ---------------------------------------------------------------------------

/// The caller of a request whose handler replies and stops reads the reply
/// only after `on_stopped` returned and the name is free.
#[test]
fn reply_goes_out_after_the_seal() {
    common::run(2, async {
        for reason in [StopReason::Graceful, shutdown()] {
            for form in [Form::Send, Form::SendPriority] {
                let hook = Hook::new();
                let rig = Rig::start(hook.clone(), false, true).await;
                let caller = ask(&rig, form, Msg::ReplyAndStop(reason.clone()));
                hook.entered().await;
                tokio::time::sleep(WINDOW).await;
                assert!(
                    !caller.is_finished(),
                    "{reason:?} {form:?}: the reply went out before the seal"
                );
                assert!(
                    rig.sys.get::<Stopper>(&rig.name).is_some(),
                    "the name stays registered until the seal"
                );
                hook.release();
                let seen = timeout(BOUND, caller)
                    .await
                    .expect("the call returns")
                    .expect("the caller task");
                assert!(matches!(seen.answer, Ok(42)), "{seen:?}");
                assert!(seen.hook_returned, "{reason:?} {form:?}: {seen:?}");
                assert!(!seen.name_registered, "{reason:?} {form:?}: {seen:?}");
                assert_eq!(rig.ended().await, reason);
            }
        }
    });
}

/// A `pre_stop` that vetoes the actor's own stop leaves it running, and the
/// reply it held is sent at once.
#[test]
fn veto_releases_the_reply_at_once() {
    common::run(2, async {
        for reason in [StopReason::Graceful, shutdown()] {
            let hook = Hook::open();
            let rig = Rig::start(hook.clone(), true, false).await;
            let answer = timeout(PROMPT, rig.handle.send(Msg::ReplyAndStop(reason.clone())))
                .await
                .unwrap_or_else(|_| panic!("{reason:?}: the reply is held after a veto"));
            assert!(matches!(answer, Ok(42)), "{answer:?}");
            assert!(rig.handle.is_alive(), "{reason:?}");
            assert!(!hook.ran.load(Ordering::SeqCst), "{reason:?}");
            assert_eq!(*hook.asked.lock().unwrap(), vec![reason.clone()]);
            let again = timeout(PROMPT, rig.handle.send(Msg::Ping))
                .await
                .expect("the actor still answers");
            assert!(matches!(again, Ok(7)), "{again:?}");
            rig.handle
                .stop(StopReason::Cancelled)
                .await
                .expect("accepted");
            assert_eq!(rig.ended().await, StopReason::Cancelled);
        }
    });
}

/// A stop a hook made through the context and a veto then refused leaves the
/// actor running: the next turn, taken from the priority lane, replies at once
/// and does not hold its reply for a stop it did not make.
#[test]
fn a_hook_stop_a_veto_refused_holds_no_later_priority_reply() {
    common::run(2, async {
        let hook = Hook::open();
        hook.pre_stop_stops.store(true, Ordering::SeqCst);
        let rig = Rig::start(hook.clone(), true, true).await;
        rig.handle
            .stop(StopReason::Graceful)
            .await
            .expect("accepted");
        let give_up = std::time::Instant::now() + BOUND;
        while hook.asked.lock().unwrap().is_empty() && std::time::Instant::now() < give_up {
            tokio::task::yield_now().await;
        }
        assert!(!hook.asked.lock().unwrap().is_empty(), "pre_stop was asked");
        let answer = timeout(PROMPT, rig.handle.send_priority(Msg::Ping))
            .await
            .expect("the reply is not held for a stop the turn did not make");
        assert!(matches!(answer, Ok(7)), "{answer:?}");
        assert!(rig.handle.is_alive());
        rig.handle
            .stop(StopReason::Cancelled)
            .await
            .expect("accepted");
        assert_eq!(rig.ended().await, StopReason::Cancelled);
    });
}

/// A `Kill` that ends the actor while its reply is held gives the caller
/// `ResponseDropped` with `Kill`.
#[test]
fn kill_first_gives_response_dropped_kill() {
    common::run(2, async {
        let hook = Hook::new();
        let rig = Rig::start(hook.clone(), false, false).await;
        let caller = ask(&rig, Form::Send, Msg::ReplyAndStop(StopReason::Graceful));
        hook.entered().await;
        rig.handle
            .stop(StopReason::Kill)
            .await
            .expect("a Kill is accepted until the seal");
        hook.release();
        let seen = timeout(BOUND, caller)
            .await
            .expect("the call returns")
            .expect("the caller task");
        assert!(
            matches!(
                seen.answer,
                Err(AskError::ResponseDropped {
                    reason: StopReason::Kill
                })
            ),
            "{seen:?}"
        );
        assert_eq!(rig.ended().await, StopReason::Kill);
    });
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// A `Kill` through the context is refused and writes nothing; the actor
/// keeps running. A stop made from `on_stopped` is accepted and changes
/// nothing.
#[test]
fn kill_is_refused() {
    common::run(2, async {
        let hook = Hook::open();
        let rig = Rig::start(hook.clone(), false, false).await;
        let answer = timeout(PROMPT, rig.handle.send(Msg::TryKill))
            .await
            .expect("the call returns");
        assert!(matches!(answer, Ok(1)), "the refusal: {answer:?}");
        let again = timeout(PROMPT, rig.handle.send(Msg::Ping))
            .await
            .expect("the actor still answers");
        assert!(matches!(again, Ok(7)), "{again:?}");
        assert!(rig.handle.is_alive());
        rig.handle
            .stop(StopReason::Graceful)
            .await
            .expect("accepted");
        assert_eq!(rig.ended().await, StopReason::Graceful);
        assert_eq!(
            *hook.late_stop.lock().unwrap(),
            Some(Ok(())),
            "a stop from on_stopped is accepted and changes nothing"
        );
    });
}

/// A stop request from outside that lands after the actor's own `Shutdown`
/// term, at the same severity, does not replace the term: the first request
/// of a rank stands, so `on_stopped` and the exit reason read the term.
#[test]
fn an_outside_graceful_never_replaces_the_actors_own_shutdown_term() {
    common::run(2, async {
        let hook = Hook::open();
        let rig = Rig::start(hook.clone(), false, false).await;
        let caller = ask(&rig, Form::Send, Msg::StopThenHold(shutdown()));
        hook.in_handler().await;
        rig.handle
            .stop(StopReason::Graceful)
            .await
            .expect("accepted");
        hook.release_handler();
        let seen = timeout(BOUND, caller)
            .await
            .expect("the call returns")
            .expect("the caller task");
        assert!(matches!(seen.answer, Ok(42)), "{seen:?}");
        assert_eq!(rig.ended().await, shutdown());
        assert_eq!(
            hook.stopped_with.lock().unwrap().clone(),
            Some(shutdown()),
            "on_stopped reads the actor's own term"
        );
    });
}
