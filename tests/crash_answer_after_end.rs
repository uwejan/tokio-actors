#![allow(dead_code)]
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
