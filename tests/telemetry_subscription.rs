//! The public surface of an event subscription: `subscribe` and
//! `unsubscribe` on a running actor's context, the ids they hand out, and the
//! end of a subscription with its actor and its system.

use tokio_actors::telemetry::{Filter, Scope, SubscriptionId};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem,
};

mod common;

fn uname(base: &str) -> String {
    format!("telemetry-subscription-{base}-{}", std::process::id())
}

enum Cmd {
    Subscribe,
    Unsubscribe(SubscriptionId),
    Event,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Id(SubscriptionId),
    Removed(bool),
}

/// Subscribes and unsubscribes on command; the events it receives are
/// ignored.
struct Watcher;

impl Actor for Watcher {
    type Message = Cmd;
    type Response = Answer;

    async fn handle(&mut self, msg: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Answer> {
        Ok(match msg {
            Cmd::Subscribe => Answer::Id(ctx.subscribe(Filter::new(Scope::System), |_| Cmd::Event)),
            Cmd::Unsubscribe(id) => Answer::Removed(ctx.unsubscribe(id)),
            Cmd::Event => Answer::Removed(false),
        })
    }
}

#[test]
fn ids_are_distinct_and_unsubscribe_answers_for_a_held_id_only() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("ids")).unwrap();
        let a = Watcher.spawn().on_system(&sys).await.unwrap();
        let b = Watcher.spawn().on_system(&sys).await.unwrap();
        let Answer::Id(first) = a.send(Cmd::Subscribe).await.unwrap() else {
            unreachable!()
        };
        let Answer::Id(second) = a.send(Cmd::Subscribe).await.unwrap() else {
            unreachable!()
        };
        assert_ne!(first, second);
        // (watcher, id, answer)
        let rows = [
            (&b, first, false),
            (&a, first, true),
            (&a, first, false),
            (&a, second, true),
        ];
        for (watcher, id, expected) in rows {
            let answer = watcher.send(Cmd::Unsubscribe(id)).await.unwrap();
            assert_eq!(answer, Answer::Removed(expected), "unsubscribe {id:?}");
        }
    });
}
