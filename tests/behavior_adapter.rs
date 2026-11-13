#![allow(dead_code)]
//! The behavior adapter: an actor whose handler returns its next state.
//!
//! A `Machine` behavior has three states. `Idle` adds, `Waiting` holds a
//! request it made and `Done` keeps the answer, so a test sees which state
//! handled a message from the response.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ActorSystem, Alias, AliasOptions, AskError, Behavior,
    BehaviorActor, BehaviorContext, Next, RequestReply, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("behavior-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

/// The actor the machine asks.
struct Doubler;

impl Actor for Doubler {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, msg: u32, _: &mut ActorContext<Self>) -> ActorResult<u32> {
        Ok(msg * 2)
    }
}

enum Cmd {
    Add(u32),
    Total,
    Ask(ActorHandle<Doubler>, u32),
    Doubled(RequestReply<u32>),
    Finish(StopReason),
    Boom,
}

#[derive(Debug, PartialEq, Eq)]
enum Out {
    /// The state that handled the message and its total.
    Idle(u32),
    Waiting(u32),
    Done(u32),
}

enum Machine {
    Idle { total: u32 },
    Waiting { total: u32 },
    Done { total: u32 },
}

impl Machine {
    fn out(&self) -> Out {
        match *self {
            Machine::Idle { total } => Out::Idle(total),
            Machine::Waiting { total } => Out::Waiting(total),
            Machine::Done { total } => Out::Done(total),
        }
    }
}

impl Behavior for Machine {
    type Message = Cmd;
    type Response = Out;

    fn handle(self, msg: Cmd, cx: &mut BehaviorContext<'_, Self>) -> (Next<Self>, Out) {
        match (self, msg) {
            (Machine::Idle { total }, Cmd::Add(n)) => {
                let total = total + n;
                (Next::Become(Machine::Idle { total }), Out::Idle(total))
            }
            (Machine::Idle { total }, Cmd::Ask(doubler, value)) => {
                cx.request(&doubler, value, Cmd::Doubled)
                    .expect("the machine is running");
                (
                    Next::Become(Machine::Waiting { total }),
                    Out::Waiting(total),
                )
            }
            (Machine::Waiting { total }, Cmd::Doubled(reply)) => {
                let total = total + reply.result.expect("the doubler answers");
                (Next::Become(Machine::Done { total }), Out::Done(total))
            }
            (state, Cmd::Finish(reason)) => {
                let out = state.out();
                (Next::Stop(reason), out)
            }
            (_, Cmd::Boom) => panic!("boom"),
            (state, _) => {
                let out = state.out();
                (Next::Become(state), out)
            }
        }
    }
}

/// `Become` replaces the state, and the response is the value the handler
/// returned for the state that handled the message.
#[test]
fn become_replaces_the_state_and_replies_with_the_returned_value() {
    common::run(2, async {
        let machine = BehaviorActor::new(Machine::Idle { total: 0 })
            .spawn()
            .await
            .unwrap();
        for (cmd, expected) in [
            (Cmd::Add(2), Out::Idle(2)),
            (Cmd::Add(3), Out::Idle(5)),
            (Cmd::Total, Out::Idle(5)),
        ] {
            assert_eq!(
                within("a reply", machine.send(cmd)).await.unwrap(),
                expected
            );
        }
    });
}

/// `Stop` ends the actor with its reason, and the caller of the message that
/// returned it still gets the response, after the stop is sealed.
#[test]
fn stop_holds_the_reply_to_the_seal() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("sys")).expect("a fresh system");
        for reason in [StopReason::Graceful, StopReason::Shutdown("done".into())] {
            let name = uname("stop");
            let machine = BehaviorActor::new(Machine::Idle { total: 4 })
                .spawn()
                .named(name.clone())
                .on_system(&sys)
                .await
                .unwrap();
            let answer = within("the reply", machine.send(Cmd::Finish(reason.clone())))
                .await
                .expect("the caller gets the response, not a dropped reply");
            assert_eq!(answer, Out::Idle(4));
            // The reply went out after the seal: the name is already free and
            // the reason is final.
            assert!(
                sys.get::<BehaviorActor<Machine>>(&name).is_none(),
                "{reason:?}: the name was still registered when the reply arrived"
            );
            assert_eq!(within("the end", machine.wait_stopped()).await, reason);
        }
    });
}

/// `Next::Stop(Kill)` is refused as `ctx.stop(Kill)` is: the actor fails with
/// an error.
#[test]
fn stop_with_kill_fails_the_actor() {
    common::run(2, async {
        let machine = BehaviorActor::new(Machine::Idle { total: 0 })
            .spawn()
            .await
            .unwrap();
        let answer = within("the reply", machine.send(Cmd::Finish(StopReason::Kill))).await;
        assert!(
            matches!(answer, Err(AskError::Actor(ActorError::User(_)))),
            "{answer:?}"
        );
        assert!(matches!(
            within("the end", machine.wait_stopped()).await,
            StopReason::Failure(ActorError::User(_))
        ));
    });
}

/// A panic in `handle` fails the actor like any handler panic.
#[test]
fn a_panic_in_handle_fails_the_actor() {
    common::run(2, async {
        let machine = BehaviorActor::new(Machine::Idle { total: 0 })
            .spawn()
            .await
            .unwrap();
        let answer = within("the reply", machine.send(Cmd::Boom)).await;
        assert!(
            matches!(&answer, Err(AskError::Actor(ActorError::Panic(message))) if message.contains("boom")),
            "{answer:?}"
        );
        assert!(matches!(
            within("the end", machine.wait_stopped()).await,
            StopReason::Failure(ActorError::Panic(_))
        ));
    });
}

// ---------------------------------------------------------------------------
// Alias, continuation and request in one behavior; a behavior as a child
// ---------------------------------------------------------------------------

type Journal = Arc<Mutex<Vec<String>>>;
type AliasSlot = Arc<Mutex<Option<Alias<BehaviorActor<Chain>>>>>;

enum Link {
    Start(ActorHandle<Doubler>),
    Step,
    Doubled(RequestReply<u32>),
    Ping,
}

/// Makes an alias, asks the doubler and queues a step, all in one handler.
struct Chain {
    journal: Journal,
    alias: AliasSlot,
}

impl Behavior for Chain {
    type Message = Link;
    type Response = ();

    fn handle(self, msg: Link, cx: &mut BehaviorContext<'_, Self>) -> (Next<Self>, ()) {
        match msg {
            Link::Start(doubler) => {
                self.journal.lock().unwrap().push("start".into());
                *self.alias.lock().unwrap() = Some(cx.alias(AliasOptions::default()));
                cx.request(&doubler, 21, Link::Doubled)
                    .expect("the chain is running");
                cx.continue_with(Link::Step);
            }
            Link::Step => self.journal.lock().unwrap().push("step".into()),
            Link::Doubled(reply) => self
                .journal
                .lock()
                .unwrap()
                .push(format!("doubled:{}", reply.result.expect("answered"))),
            Link::Ping => self.journal.lock().unwrap().push("ping".into()),
        }
        (Next::Become(self), ())
    }
}

async fn journal_has(journal: &Journal, entry: &str) {
    within("a journal entry", async {
        while !journal.lock().unwrap().iter().any(|e| e == entry) {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// One handler uses `alias`, `continue_with` and `request` together: the
/// continuation runs before the reply, which is an ordinary message, and the
/// alias reaches the same behavior afterwards.
#[test]
fn a_behavior_uses_an_alias_a_continuation_and_a_request_together() {
    common::run(2, async {
        let journal: Journal = Arc::default();
        let alias: AliasSlot = Arc::default();
        let chain = BehaviorActor::new(Chain {
            journal: journal.clone(),
            alias: alias.clone(),
        })
        .spawn()
        .await
        .unwrap();
        let doubler = Doubler.spawn().await.unwrap();
        within("the start", chain.send(Link::Start(doubler)))
            .await
            .unwrap();
        journal_has(&journal, "doubled:42").await;
        let alias = alias.lock().unwrap().clone().expect("the handler made one");
        assert!(alias.is_active());
        within("the alias send", alias.notify(Link::Ping))
            .await
            .unwrap();
        journal_has(&journal, "ping").await;
        assert_eq!(
            *journal.lock().unwrap(),
            ["start", "step", "doubled:42", "ping"]
        );
    });
}

/// The supervisor of a behavior child.
struct Boss;

impl Actor for Boss {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| BehaviorActor::new(Machine::Idle { total: 0 }))
            .named("machine")
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}
