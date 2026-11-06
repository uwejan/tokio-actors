//! A request whose handler fails is answered once the actor has ended: its
//! stop lane is sealed and its name is free, the way an Erlang/OTP
//! `gen_server:call` caller reads a crash after the process is dead. Until
//! then the name stays registered to the ending incarnation. A `Kill` or an
//! abort that ends the actor first, and the end of the actor's runtime, leave
//! the caller to read the final reason, `Kill`.
//!
//! Nothing is ordered by a sleep: the actors hold their `on_stopped` at gates
//! the tests open, and every call records what the world looked like the
//! moment it returned. The windows only give an early answer time to arrive,
//! and the bounds only detect a hang.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{oneshot, Notify, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorId, ActorResult, ActorStatus, ActorSystem, AskError,
    Shutdown, SpawnBuilder, StopReason,
};

mod common;

/// The longest a wait may take. It only limits how long a real hang takes to
/// report.
const BOUND: Duration = Duration::from_secs(60);

/// How long a call that must still be waiting is watched: long enough for an
/// early answer to have arrived.
const WINDOW: Duration = Duration::from_millis(200);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "crash-answer-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn fresh_system(tag: &str) -> Arc<ActorSystem> {
    ActorSystem::create(uname(tag)).expect("a fresh system")
}

/// Polls a call once, which enqueues its message, and returns whether it is
/// still waiting for the answer.
fn poll_once<F: Future + ?Sized>(call: Pin<&mut F>) -> bool {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    call.poll(&mut cx).is_pending()
}

/// Waits, by observation, until `condition` holds.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let waited = timeout(BOUND, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "never observed: {what}");
}

/// What a test holds and watches of an actor's end.
struct Hook {
    /// Set when `on_stopped` begins.
    entered: Notify,
    /// `on_stopped` waits for a permit.
    gate: Semaphore,
    /// Set when `on_stopped` has returned.
    returned: AtomicBool,
    /// Set when `on_stopped` has begun, and stays set.
    ran: AtomicBool,
    /// Set when a handler turn that holds itself begins.
    in_handler: Notify,
    /// A handler turn that holds itself waits for a permit.
    handler_gate: Semaphore,
}

impl Hook {
    fn new() -> Arc<Hook> {
        Arc::new(Hook {
            entered: Notify::new(),
            gate: Semaphore::new(0),
            returned: AtomicBool::new(false),
            ran: AtomicBool::new(false),
            in_handler: Notify::new(),
            handler_gate: Semaphore::new(0),
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
}

#[derive(Clone, Copy, Debug)]
enum Msg {
    Ping,
    /// The handler returns an error.
    Fail,
    /// The handler panics.
    Panic,
    /// The handler waits at its gate, then returns an error.
    SlowFail,
}

fn respond(msg: Msg) -> ActorResult<u32> {
    match msg {
        Msg::Ping => Ok(1),
        Msg::Fail | Msg::SlowFail => Err(ActorError::user("handler failed")),
        Msg::Panic => panic!("handler panicked"),
    }
}

/// An actor whose `on_stopped` waits at a gate.
struct Gated {
    hook: Arc<Hook>,
}

impl Actor for Gated {
    type Message = Msg;
    type Response = u32;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        if let Msg::SlowFail = msg {
            self.hook.in_handler.notify_one();
            self.hook
                .handler_gate
                .acquire()
                .await
                .expect("the gate stays open")
                .forget();
        }
        respond(msg)
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
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

/// A supervisor with one child whose `on_stopped` waits at a gate: the end of
/// the supervisor includes the child's teardown.
struct Boss {
    child: Arc<Hook>,
    child_name: String,
}

impl Actor for Boss {
    type Message = Msg;
    type Response = u32;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let hook = self.child.clone();
        ctx.spawn_child(move || Gated { hook: hook.clone() })
            .named(self.child_name.clone())
            .await?;
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        respond(msg)
    }
}

/// How a request is made.
#[derive(Clone, Copy, Debug)]
enum Form {
    Send,
    SendPriority,
    SendTimeout(Duration),
    SendPriorityTimeout(Duration),
}

impl Form {
    async fn call<A: Actor<Message = Msg, Response = u32>>(
        self,
        handle: &ActorHandle<A>,
        msg: Msg,
    ) -> Result<u32, AskError> {
        match self {
            Form::Send => handle.send(msg).await,
            Form::SendPriority => handle.send_priority(msg).await,
            Form::SendTimeout(within) => handle.send_timeout(msg, within).await,
            Form::SendPriorityTimeout(within) => handle.send_priority_timeout(msg, within).await,
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
    /// The handle still reported the actor alive.
    alive: bool,
}

/// An actor on its own system, the hook the test holds, and the callers.
struct Rig<A: Actor<Message = Msg, Response = u32>> {
    sys: Arc<ActorSystem>,
    name: String,
    hook: Arc<Hook>,
    handle: ActorHandle<A>,
}

impl<A: Actor<Message = Msg, Response = u32>> Rig<A> {
    async fn start(
        actor: A,
        hook: Arc<Hook>,
        tune: impl FnOnce(SpawnBuilder<A>) -> SpawnBuilder<A>,
    ) -> Rig<A> {
        let sys = fresh_system("rig");
        let name = uname("actor");
        let builder = actor.spawn().named(name.clone()).on_system(&sys);
        let handle = tune(builder).await.expect("the actor starts");
        Rig {
            sys,
            name,
            hook,
            handle,
        }
    }

    /// Makes the call in a task of its own and records what the world looks
    /// like when it returns.
    fn ask(&self, form: Form, msg: Msg) -> JoinHandle<Seen> {
        let (sys, name, hook, handle) = (
            self.sys.clone(),
            self.name.clone(),
            self.hook.clone(),
            self.handle.clone(),
        );
        tokio::spawn(async move {
            let answer = form.call(&handle, msg).await;
            Seen {
                answer,
                hook_returned: hook.returned.load(Ordering::SeqCst),
                name_registered: sys.get::<A>(&name).is_some(),
                alive: handle.is_alive(),
            }
        })
    }

    fn lookup(&self) -> Option<ActorHandle<A>> {
        self.sys.get::<A>(&self.name)
    }
}

impl Rig<Gated> {
    async fn gated(tune: impl FnOnce(SpawnBuilder<Gated>) -> SpawnBuilder<Gated>) -> Self {
        let hook = Hook::new();
        Rig::start(Gated { hook: hook.clone() }, hook, tune).await
    }
}

fn with_priority_lane(builder: SpawnBuilder<Gated>) -> SpawnBuilder<Gated> {
    builder.with_config(ActorConfig::default().with_priority_lane(8))
}

fn failed(err: &AskError) -> bool {
    matches!(err, AskError::Actor(ActorError::User(_)))
}

fn panicked(err: &AskError) -> bool {
    matches!(err, AskError::Actor(ActorError::Panic(_)))
}

fn dropped_with_a_failure(answer: &Result<u32, AskError>) -> bool {
    matches!(
        answer,
        Err(AskError::ResponseDropped {
            reason: StopReason::Failure(_)
        })
    )
}

fn dropped_with_kill(answer: &Result<u32, AskError>) -> bool {
    matches!(
        answer,
        Err(AskError::ResponseDropped {
            reason: StopReason::Kill
        })
    )
}

/// The failing request is held through `on_stopped`: the name stays
/// registered to the ending incarnation, which still accepts a request and
/// drops it unhandled, and the caller is answered only when `on_stopped` has
/// returned and the name is free.
async fn failing_request_is_answered_after_the_end(
    form: Form,
    msg: Msg,
    tune: fn(SpawnBuilder<Gated>) -> SpawnBuilder<Gated>,
    expect: fn(&AskError) -> bool,
) {
    let rig = Rig::gated(tune).await;
    let mut ask = rig.ask(form, msg);
    rig.hook.entered().await;

    let found = rig
        .lookup()
        .expect("the name stays registered to the ending incarnation");
    assert_eq!(found.status(), ActorStatus::Stopping);
    assert!(
        found.is_alive(),
        "its mailbox is open while on_stopped runs"
    );
    let mut late = Box::pin(found.send(Msg::Ping));
    assert!(poll_once(late.as_mut()), "a late request is accepted");
    assert!(
        timeout(WINDOW, &mut ask).await.is_err(),
        "the failing request was answered before the actor ended"
    );

    rig.hook.release();
    let seen = timeout(BOUND, ask)
        .await
        .expect("the answer arrives once the actor has ended")
        .unwrap();
    let err = seen.answer.as_ref().expect_err("the handler failed");
    assert!(expect(err), "{err:?}");
    assert!(seen.hook_returned, "answered before on_stopped returned");
    assert!(!seen.name_registered, "answered with the name registered");
    assert!(!seen.alive, "answered while the handle reads alive");

    // The request that reached the ending incarnation meanwhile is dropped
    // unhandled, with the actor's final reason.
    let late = timeout(BOUND, late).await.expect("the late request ends");
    assert!(dropped_with_a_failure(&late), "{late:?}");
    assert!(matches!(
        rig.handle.wait_stopped().await,
        StopReason::Failure(_)
    ));
}

#[test]
fn an_err_is_answered_only_after_the_actor_has_ended() {
    common::run(0, async {
        failing_request_is_answered_after_the_end(Form::Send, Msg::Fail, |b| b, failed).await;
    });
}

#[test]
fn a_panic_is_answered_only_after_the_actor_has_ended() {
    common::run(0, async {
        failing_request_is_answered_after_the_end(Form::Send, Msg::Panic, |b| b, panicked).await;
    });
}

#[test]
fn a_failing_priority_request_is_answered_after_the_actor_has_ended() {
    common::run(0, async {
        failing_request_is_answered_after_the_end(
            Form::SendPriority,
            Msg::Fail,
            with_priority_lane,
            failed,
        )
        .await;
    });
}

#[test]
fn a_failing_timed_priority_request_is_answered_after_the_actor_has_ended() {
    common::run(0, async {
        failing_request_is_answered_after_the_end(
            Form::SendPriorityTimeout(BOUND),
            Msg::Fail,
            with_priority_lane,
            failed,
        )
        .await;
    });
}

#[test]
fn a_failing_timed_request_is_answered_after_the_actor_has_ended() {
    common::run(0, async {
        failing_request_is_answered_after_the_end(
            Form::SendTimeout(BOUND),
            Msg::Fail,
            |b| b,
            failed,
        )
        .await;
    });
}

/// The answer waits for the teardown of the actor's children: while a child
/// of the failed supervisor is still stopping, the supervisor's name is
/// registered, it reads stopping, and it accepts nothing.
#[test]
fn the_answer_waits_for_the_childrens_teardown() {
    common::run(0, async {
        let child = Hook::new();
        let boss = Boss {
            child: child.clone(),
            child_name: uname("child"),
        };
        let rig = Rig::start(boss, child.clone(), |b| b.supervisor()).await;
        let ask = rig.ask(Form::Send, Msg::Fail);
        child.entered().await;

        let found = rig
            .lookup()
            .expect("the supervisor's name stays registered");
        assert!(
            !found.is_alive(),
            "its lanes are closed once its own on_stopped returned"
        );
        assert_eq!(found.status(), ActorStatus::Stopping);
        assert!(matches!(found.send(Msg::Ping).await, Err(AskError::Closed)));
        let mut ask = ask;
        assert!(
            timeout(WINDOW, &mut ask).await.is_err(),
            "answered while a child was still stopping"
        );

        child.release();
        let seen = timeout(BOUND, ask).await.expect("answered").unwrap();
        assert!(matches!(seen.answer, Err(AskError::Actor(_))), "{seen:?}");
        assert!(seen.hook_returned, "answered before the child had ended");
        assert!(!seen.name_registered);
    });
}

/// A `Kill` accepted while the answer is held is the final reason: the caller
/// reads it, not the handler's error.
#[test]
fn a_kill_accepted_while_the_answer_is_held_reads_the_final_reason() {
    common::run(0, async {
        let rig = Rig::gated(|b| b.kill_grace(Duration::from_secs(60))).await;
        let ask = rig.ask(Form::Send, Msg::Fail);
        rig.hook.entered().await;
        rig.handle
            .stop(StopReason::Kill)
            .await
            .expect("a Kill is accepted while on_stopped runs");
        rig.hook.release();
        let seen = timeout(BOUND, ask).await.expect("answered").unwrap();
        assert!(dropped_with_kill(&seen.answer), "{seen:?}");
        assert!(!seen.name_registered);
        assert_eq!(rig.handle.wait_stopped().await, StopReason::Kill);
    });
}

/// A `Kill` stored while the failing handler still runs ends the actor with
/// `Kill`: `on_stopped` is skipped and the caller reads the final reason.
#[test]
fn a_kill_stored_while_the_failing_handler_runs_reads_the_final_reason() {
    common::run(0, async {
        let rig = Rig::gated(|b| b.kill_grace(Duration::from_secs(60))).await;
        let ask = rig.ask(Form::Send, Msg::SlowFail);
        rig.hook.in_handler().await;
        rig.handle
            .stop(StopReason::Kill)
            .await
            .expect("a Kill is accepted while the handler runs");
        rig.hook.handler_gate.add_permits(1);
        let seen = timeout(BOUND, ask).await.expect("answered").unwrap();
        assert!(dropped_with_kill(&seen.answer), "{seen:?}");
        assert!(!seen.name_registered);
        assert_eq!(rig.handle.wait_stopped().await, StopReason::Kill);
        assert!(
            !rig.hook.ran.load(Ordering::SeqCst),
            "on_stopped ran after a Kill"
        );
    });
}

/// A `Kill` read inside the teardown of a supervisor's children, while a
/// child's `on_stopped` is held, ends the supervisor with `Kill`: its caller
/// reads the final reason.
#[test]
fn a_kill_read_in_the_childrens_teardown_reads_the_final_reason() {
    common::run(0, async {
        let child = Hook::new();
        let boss = Boss {
            child: child.clone(),
            child_name: uname("child"),
        };
        let rig = Rig::start(boss, child.clone(), |b| {
            b.supervisor().kill_grace(Duration::from_secs(60))
        })
        .await;
        let ask = rig.ask(Form::Send, Msg::Fail);
        child.entered().await;
        rig.handle
            .stop(StopReason::Kill)
            .await
            .expect("a Kill is accepted during the teardown");
        let seen = timeout(BOUND, ask).await.expect("answered").unwrap();
        assert!(dropped_with_kill(&seen.answer), "{seen:?}");
        assert!(!seen.name_registered);
        assert_eq!(rig.handle.wait_stopped().await, StopReason::Kill);
        child.release();
    });
}

/// A value the actor registered with a timer, whose drop holds at a gate once
/// the mailbox has closed.
struct HeldDrop {
    entered: Option<oneshot::Sender<()>>,
    release: Option<std::sync::mpsc::Receiver<()>>,
}

impl Drop for HeldDrop {
    fn drop(&mut self) {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        if let Some(release) = self.release.take() {
            tokio::task::block_in_place(|| {
                let _ = release.recv();
            });
        }
    }
}

struct Timered {
    owner: Option<HeldDrop>,
}

impl Actor for Timered {
    type Message = Msg;
    type Response = u32;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let owner = self.owner.take();
        ctx.every_with(Duration::from_secs(3600), move || {
            let _ = &owner;
            Msg::Ping
        })
        .await
        .expect("the timer starts");
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        respond(msg)
    }
}

/// A `Kill` accepted after the name is free and before the lane is sealed,
/// while the end waits for a timer's value to drop, is the final reason: the
/// caller reads it, not the handler's error. Multi-thread only: the held drop
/// blocks its thread.
#[test]
fn a_kill_accepted_after_the_name_is_free_reads_the_final_reason() {
    common::run_multi_thread_only(4, async {
        let (entered_tx, entered) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let hook = Hook::new();
        let rig = Rig::start(
            Timered {
                owner: Some(HeldDrop {
                    entered: Some(entered_tx),
                    release: Some(release_rx),
                }),
            },
            hook,
            |b| b.kill_grace(Duration::from_secs(60)),
        )
        .await;
        let ask = rig.ask(Form::Send, Msg::Fail);
        timeout(BOUND, entered)
            .await
            .expect("the value's drop begins")
            .unwrap();
        until("the name to be free", || rig.lookup().is_none()).await;
        rig.handle
            .stop(StopReason::Kill)
            .await
            .expect("a Kill is accepted until the lane is sealed");
        release_tx.send(()).unwrap();
        let seen = timeout(BOUND, ask).await.expect("answered").unwrap();
        assert!(dropped_with_kill(&seen.answer), "{seen:?}");
        assert_eq!(rig.handle.wait_stopped().await, StopReason::Kill);
    });
}

/// An abort that cuts the end before the seal leaves the caller the final
/// reason, read once the name is free.
#[test]
fn an_abort_before_the_seal_reads_the_final_reason_with_the_name_free() {
    common::run(0, async {
        let rig = Rig::gated(|b| b.kill_grace(Duration::from_millis(20))).await;
        let ask = rig.ask(Form::Send, Msg::Fail);
        rig.hook.entered().await;
        rig.handle
            .stop(StopReason::Kill)
            .await
            .expect("a Kill is accepted while on_stopped runs");
        // The gate is never opened: the abort cuts on_stopped at its gate.
        let seen = timeout(BOUND, ask).await.expect("answered").unwrap();
        assert!(dropped_with_kill(&seen.answer), "{seen:?}");
        assert!(!seen.name_registered, "the name was not free yet");
        assert!(!seen.hook_returned);
    });
}

/// The end of the runtime the actor runs on leaves the caller, on another
/// runtime, the final reason, with the name free. Multi-thread only: the
/// runtime is shut down from a thread of its own.
#[test]
fn the_end_of_the_actors_runtime_releases_the_caller() {
    common::run_multi_thread_only(2, async {
        let sys = fresh_system("runtime-end");
        let name = uname("actor");
        let hook = Hook::new();
        let actor_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let handle = {
            let (sys, name, hook) = (sys.clone(), name.clone(), hook.clone());
            actor_rt
                .spawn(async move {
                    Gated { hook }
                        .spawn()
                        .named(name)
                        .on_system(&sys)
                        .await
                        .expect("the actor starts")
                })
                .await
                .unwrap()
        };
        let asker = {
            let (sys, name, handle) = (sys.clone(), name.clone(), handle.clone());
            tokio::spawn(async move {
                let answer = handle.send(Msg::Fail).await;
                (answer, sys.get::<Gated>(&name).is_some())
            })
        };
        hook.entered().await;
        let ending = std::thread::spawn(move || drop(actor_rt));
        let (answer, registered) = timeout(BOUND, asker).await.expect("answered").unwrap();
        assert!(dropped_with_kill(&answer), "{answer:?}");
        assert!(!registered, "the name was not free yet");
        ending.join().unwrap();
    });
}

/// A worker whose `on_stopped` takes `delay`, and which supervises a child of
/// its own when `leaf` is set, so its end includes that child's teardown.
struct Worker {
    delay: Duration,
    leaf: bool,
}

struct Leaf;

impl Actor for Leaf {
    type Message = Msg;
    type Response = u32;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        respond(msg)
    }
}

impl Actor for Worker {
    type Message = Msg;
    type Response = u32;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.leaf {
            ctx.spawn_child(|| Leaf).await?;
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        respond(msg)
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(())
    }
}

/// A supervisor that starts one worker, named or anonymous, and publishes the
/// worker's id.
struct Sup {
    name: Option<String>,
    delay: Duration,
    leaf: bool,
    id: Arc<Mutex<Option<ActorId>>>,
}

impl Actor for Sup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (delay, leaf) = (self.delay, self.leaf);
        let spawn = ctx.spawn_child(move || Worker { delay, leaf });
        let spawn = if leaf {
            spawn.with_config(ActorConfig::default().supervisor())
        } else {
            spawn
        };
        let child = match &self.name {
            Some(name) => spawn.named(name.clone()).await?,
            None => spawn.await?,
        };
        *self.id.lock().unwrap() = Some(child.id().clone());
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

const ITERATIONS: usize = 30;

/// How a lookup is made: by name or by id.
#[derive(Clone, Copy)]
enum Lookup {
    Name,
    Id,
}

/// Each iteration crashes a supervised worker with a call and looks it up the
/// moment the call returns: it finds nothing yet or the next incarnation,
/// which answers, never the incarnation that crashed.
async fn a_retry_after_the_answer_never_meets_the_crashed_incarnation(
    delay: Duration,
    leaf: bool,
    by: Lookup,
) {
    let sys = fresh_system("retry");
    for iteration in 0..ITERATIONS {
        let name = uname("worker");
        let id = Arc::new(Mutex::new(None));
        let sup = Sup {
            name: matches!(by, Lookup::Name).then(|| name.clone()),
            delay,
            leaf,
            id: id.clone(),
        }
        .spawn()
        .named(uname("sup"))
        .on_system(&sys)
        .supervisor()
        .await
        .expect("the supervisor starts");
        let id = id.lock().unwrap().clone().expect("the worker started");
        let find = || match by {
            Lookup::Name => sys.get::<Worker>(&name),
            Lookup::Id => sys.get_by_id::<Worker>(&id),
        };
        let worker = find().expect("the worker is registered");

        let answer = timeout(BOUND, worker.send(Msg::Fail))
            .await
            .expect("the call is answered");
        assert!(
            matches!(&answer, Err(AskError::Actor(ActorError::User(_)))),
            "iteration {iteration}: {answer:?}"
        );
        // The lookup is made at once: nothing yet, or the next incarnation.
        let retried = timeout(BOUND, async {
            loop {
                match find() {
                    None => tokio::task::yield_now().await,
                    Some(next) => break next.send(Msg::Ping).await,
                }
            }
        })
        .await
        .expect("the successor is registered");
        assert!(
            matches!(retried, Ok(1)),
            "iteration {iteration}: a retry met the crashed incarnation: {retried:?}"
        );
        sup.stop(StopReason::Graceful).await.expect("accepted");
        timeout(BOUND, sup.wait_stopped())
            .await
            .expect("the supervisor ends");
    }
}

#[test]
fn a_retry_by_name_after_an_instant_end_never_meets_the_crashed_incarnation() {
    common::run(0, async {
        a_retry_after_the_answer_never_meets_the_crashed_incarnation(
            Duration::ZERO,
            false,
            Lookup::Name,
        )
        .await;
    });
}

#[test]
fn a_retry_by_name_after_a_slow_end_never_meets_the_crashed_incarnation() {
    common::run(0, async {
        a_retry_after_the_answer_never_meets_the_crashed_incarnation(
            Duration::from_millis(5),
            false,
            Lookup::Name,
        )
        .await;
    });
}

#[test]
fn a_retry_by_name_after_an_instant_end_with_a_child_never_meets_the_crashed_incarnation() {
    common::run(0, async {
        a_retry_after_the_answer_never_meets_the_crashed_incarnation(
            Duration::ZERO,
            true,
            Lookup::Name,
        )
        .await;
    });
}

#[test]
fn a_retry_by_name_after_a_slow_end_with_a_child_never_meets_the_crashed_incarnation() {
    common::run(0, async {
        a_retry_after_the_answer_never_meets_the_crashed_incarnation(
            Duration::from_millis(5),
            true,
            Lookup::Name,
        )
        .await;
    });
}

#[test]
fn a_retry_by_id_after_a_slow_end_never_meets_the_crashed_incarnation() {
    common::run(0, async {
        a_retry_after_the_answer_never_meets_the_crashed_incarnation(
            Duration::from_millis(5),
            false,
            Lookup::Id,
        )
        .await;
    });
}

#[test]
fn a_retry_by_id_after_an_instant_end_with_a_child_never_meets_the_crashed_incarnation() {
    common::run(0, async {
        a_retry_after_the_answer_never_meets_the_crashed_incarnation(
            Duration::ZERO,
            true,
            Lookup::Id,
        )
        .await;
    });
}

/// A child of the failing actor whose handler waits on the call cannot take
/// its parent's stop while the call is held: its shutdown window runs out
/// and it is killed, without its `on_stopped`, and the parent's end is
/// bounded by that window and the kill grace.
struct Waiter {
    sys: Arc<ActorSystem>,
    parent: String,
    ran_on_stopped: Arc<AtomicBool>,
}

enum WaiterMsg {
    /// Calls the parent with a request that fails.
    AskParent,
}

impl Actor for Waiter {
    type Message = WaiterMsg;
    type Response = ();

    async fn handle(&mut self, _msg: WaiterMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let parent = self.sys.get::<Elder>(&self.parent).expect("the parent");
        let _ = parent.send(Msg::Fail).await;
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.ran_on_stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct Elder {
    sys: Arc<ActorSystem>,
    name: String,
    child: String,
    ran_on_stopped: Arc<AtomicBool>,
}

impl Actor for Elder {
    type Message = Msg;
    type Response = u32;

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (sys, parent, ran) = (
            self.sys.clone(),
            self.name.clone(),
            self.ran_on_stopped.clone(),
        );
        ctx.spawn_child(move || Waiter {
            sys: sys.clone(),
            parent: parent.clone(),
            ran_on_stopped: ran.clone(),
        })
        .named(self.child.clone())
        .shutdown(Shutdown::Timeout(Duration::from_millis(100)))
        .kill_grace(Duration::from_millis(50))
        .await?;
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        respond(msg)
    }
}

#[test]
fn a_child_waiting_on_its_failing_parent_is_ended_by_its_shutdown() {
    common::run(0, async {
        let sys = fresh_system("waiting-child");
        let (name, child) = (uname("elder"), uname("waiter"));
        let ran = Arc::new(AtomicBool::new(false));
        let elder = Elder {
            sys: sys.clone(),
            name: name.clone(),
            child: child.clone(),
            ran_on_stopped: ran.clone(),
        }
        .spawn()
        .named(name)
        .on_system(&sys)
        .supervisor()
        .await
        .expect("the parent starts");
        let waiter = sys.get::<Waiter>(&child).expect("the child");
        waiter.notify(WaiterMsg::AskParent).await.unwrap();

        let parent_end = timeout(BOUND, elder.wait_stopped())
            .await
            .expect("the parent's end is bounded by its child's shutdown window");
        assert!(
            matches!(parent_end, StopReason::Failure(_)),
            "{parent_end:?}"
        );
        let child_end = timeout(BOUND, waiter.wait_stopped())
            .await
            .expect("the child ends");
        assert_eq!(child_end, StopReason::Kill, "the child is killed");
        assert!(
            !ran.load(Ordering::SeqCst),
            "the child's on_stopped ran: it was stopped in order"
        );
    });
}

/// The wait for the answer counts against the call's own deadline.
async fn a_timed_call_counts_the_teardown(
    form: fn(Duration) -> Form,
    tune: fn(SpawnBuilder<Gated>) -> SpawnBuilder<Gated>,
) {
    // The gate stays shut until the call has returned.
    let rig = Rig::gated(tune).await;
    let seen = timeout(BOUND, rig.ask(form(Duration::from_millis(100)), Msg::Fail))
        .await
        .expect("the deadline ends the call")
        .unwrap();
    assert!(
        matches!(seen.answer, Err(AskError::Timeout { enqueued: true })),
        "{seen:?}"
    );
    rig.hook.release();
    assert!(matches!(
        rig.handle.wait_stopped().await,
        StopReason::Failure(_)
    ));

    // A deadline past the end reads the handler's error.
    let rig = Rig::gated(tune).await;
    let ask = rig.ask(form(BOUND), Msg::Fail);
    rig.hook.entered().await;
    rig.hook.release();
    let seen = timeout(BOUND, ask).await.expect("answered").unwrap();
    assert!(
        matches!(seen.answer, Err(AskError::Actor(ActorError::User(_)))),
        "{seen:?}"
    );
    assert!(seen.hook_returned && !seen.name_registered);
}

#[test]
fn send_timeout_counts_the_teardown() {
    common::run(0, async {
        a_timed_call_counts_the_teardown(Form::SendTimeout, |b| b).await;
    });
}

#[test]
fn send_priority_timeout_counts_the_teardown() {
    common::run(0, async {
        a_timed_call_counts_the_teardown(Form::SendPriorityTimeout, with_priority_lane).await;
    });
}

/// Two actors that call each other: the first calls the second with a failing
/// request and a deadline, the second's `on_stopped` calls the first.
struct First {
    sys: Arc<ActorSystem>,
    second: String,
    call: Arc<Mutex<Option<Result<u32, AskError>>>>,
}

enum FirstMsg {
    Ping,
    CallSecond,
}

impl Actor for First {
    type Message = FirstMsg;
    type Response = u32;

    async fn handle(&mut self, msg: FirstMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        if let FirstMsg::CallSecond = msg {
            let second = self.sys.get::<Second>(&self.second).expect("the second");
            let result = second
                .send_timeout(Msg::Fail, Duration::from_millis(200))
                .await;
            *self.call.lock().unwrap() = Some(result);
        }
        Ok(1)
    }
}

struct Second {
    sys: Arc<ActorSystem>,
    first: String,
    ping: Arc<Mutex<Option<Result<u32, AskError>>>>,
}

impl Actor for Second {
    type Message = Msg;
    type Response = u32;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        respond(msg)
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let first = self.sys.get::<First>(&self.first).expect("the first");
        let result = first.send(FirstMsg::Ping).await;
        *self.ping.lock().unwrap() = Some(result);
        Ok(())
    }
}

/// A caller that bounds its wait breaks the cycle with the target's
/// `on_stopped`: the deadline frees the caller to answer the hook's call.
#[test]
fn a_caller_that_bounds_its_wait_breaks_a_cycle_with_on_stopped() {
    common::run(0, async {
        let sys = fresh_system("cycle");
        let (first_name, second_name) = (uname("first"), uname("second"));
        let call = Arc::new(Mutex::new(None));
        let ping = Arc::new(Mutex::new(None));
        let first = First {
            sys: sys.clone(),
            second: second_name.clone(),
            call: call.clone(),
        }
        .spawn()
        .named(first_name.clone())
        .on_system(&sys)
        .await
        .unwrap();
        let second = Second {
            sys: sys.clone(),
            first: first_name,
            ping: ping.clone(),
        }
        .spawn()
        .named(second_name)
        .on_system(&sys)
        .await
        .unwrap();

        first.send(FirstMsg::CallSecond).await.unwrap();
        let end = timeout(BOUND, second.wait_stopped())
            .await
            .expect("the second ends once the first has answered it");
        assert!(matches!(end, StopReason::Failure(_)), "{end:?}");
        let call = call.lock().unwrap().take().expect("the first's call ended");
        assert!(
            matches!(call, Err(AskError::Timeout { enqueued: true })),
            "{call:?}"
        );
        let ping = ping.lock().unwrap().take().expect("the hook's call ended");
        assert!(matches!(ping, Ok(1)), "{ping:?}");
    });
}

/// A caller that gives up leaves the end and the restart as they were.
#[test]
fn a_dropped_caller_leaves_the_end_and_the_restart_unchanged() {
    common::run(0, async {
        struct Keeper {
            name: String,
            hook: Arc<Hook>,
        }
        impl Actor for Keeper {
            type Message = ();
            type Response = ();
            async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
                let hook = self.hook.clone();
                ctx.spawn_child(move || Gated { hook: hook.clone() })
                    .named(self.name.clone())
                    .await?;
                Ok(())
            }
            async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
                Ok(())
            }
        }

        let sys = fresh_system("dropped-caller");
        let (name, hook) = (uname("worker"), Hook::new());
        let sup = Keeper {
            name: name.clone(),
            hook: hook.clone(),
        }
        .spawn()
        .named(uname("keeper"))
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
        let worker = sys.get::<Gated>(&name).expect("the worker");

        let mut ask = Box::pin(worker.send(Msg::Fail));
        tokio::select! {
            biased;
            answer = &mut ask => panic!("answered before the actor ended: {answer:?}"),
            () = hook.entered() => {}
        }
        assert!(
            timeout(WINDOW, &mut ask).await.is_err(),
            "answered while on_stopped was held"
        );
        // The caller gives up.
        drop(ask);

        hook.release();
        assert!(matches!(
            timeout(BOUND, worker.wait_stopped())
                .await
                .expect("it ends"),
            StopReason::Failure(_)
        ));
        until("the successor to register", || {
            sys.get::<Gated>(&name).is_some()
        })
        .await;
        let successor = sys.get::<Gated>(&name).unwrap();
        assert!(matches!(
            timeout(BOUND, successor.send(Msg::Ping))
                .await
                .expect("answers"),
            Ok(1)
        ));
        hook.gate.add_permits(100);
        sup.stop(StopReason::Graceful).await.unwrap();
        timeout(BOUND, sup.wait_stopped()).await.expect("ends");
    });
}

/// A request queued behind the failing one is dropped unhandled with the
/// actor's final reason, while the failing request reads its error.
#[test]
fn a_request_queued_behind_the_failing_one_reads_the_final_reason() {
    common::run(0, async {
        let hook = Hook::open();
        let rig = Rig::start(Gated { hook: hook.clone() }, hook.clone(), |b| b).await;
        let first = rig.ask(Form::Send, Msg::SlowFail);
        hook.in_handler().await;
        let mut second = Box::pin(rig.handle.send(Msg::Ping));
        assert!(poll_once(second.as_mut()), "the request is queued");
        hook.handler_gate.add_permits(1);

        let first = timeout(BOUND, first).await.expect("answered").unwrap();
        assert!(failed(first.answer.as_ref().unwrap_err()), "{first:?}");
        let second = timeout(BOUND, second).await.expect("answered");
        assert!(dropped_with_a_failure(&second), "{second:?}");
    });
}
