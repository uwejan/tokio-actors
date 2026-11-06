//! A named actor's id and name are one allocation: for a top-level actor, for
//! a supervised child, and for every incarnation a restart starts.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::{sleep, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, ActorSystem,
};

mod common;

static BORN: AtomicU64 = AtomicU64::new(0);
static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("share-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

struct Probe {
    born: u64,
}

impl Probe {
    fn new() -> Self {
        Self {
            born: BORN.fetch_add(1, Ordering::Relaxed),
        }
    }
}

enum ProbeMsg {
    Where,
    Crash,
}

/// The incarnation's number, then the address of the id's text and of the
/// name's text.
type Where = (u64, usize, usize);

impl Actor for Probe {
    type Message = ProbeMsg;
    type Response = Where;

    async fn handle(&mut self, msg: ProbeMsg, ctx: &mut ActorContext<Self>) -> ActorResult<Where> {
        match msg {
            ProbeMsg::Where => Ok((
                self.born,
                ctx.actor_id().as_str().as_ptr() as usize,
                ctx.actor_name().expect("a named actor").as_ptr() as usize,
            )),
            ProbeMsg::Crash => Err(ActorError::user("crash on request")),
        }
    }
}

struct Parent;

impl Actor for Parent {
    type Message = String;
    type Response = ();

    async fn handle(&mut self, name: String, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(Probe::new).named(name).await?;
        Ok(())
    }
}

#[test]
fn a_top_level_actor_shares_one_allocation_for_its_id_and_name() {
    common::run(0, async {
        let name = uname("top");
        let probe = Probe::new().spawn().named(name.clone()).await.unwrap();
        let (_, id_at, name_at) = probe.send(ProbeMsg::Where).await.unwrap();
        assert_eq!(id_at, name_at, "the id and the name are one allocation");
        assert_eq!(probe.id().as_str(), name);
    });
}

#[test]
fn a_child_and_each_restarted_incarnation_keep_one_shared_allocation() {
    common::run(0, async {
        let parent_name = uname("parent");
        let parent = Parent
            .spawn()
            .named(parent_name)
            .supervisor()
            .await
            .unwrap();
        let name = uname("child");
        parent.send(name.clone()).await.unwrap();
        let sys = ActorSystem::default();
        let child = sys.get::<Probe>(&name).expect("the child is registered");
        let (first_born, first_id, first_name) = child.send(ProbeMsg::Where).await.unwrap();
        assert_eq!(
            first_id, first_name,
            "the id and the name are one allocation"
        );

        assert!(child.send(ProbeMsg::Crash).await.is_err());
        let deadline = Instant::now() + Duration::from_secs(10);
        let (born, id_at, name_at) = loop {
            if let Some(next) = sys.get::<Probe>(&name) {
                if let Ok(seen) = next.send(ProbeMsg::Where).await {
                    if seen.0 != first_born {
                        break seen;
                    }
                }
            }
            assert!(Instant::now() < deadline, "the child restarted in time");
            sleep(Duration::from_millis(10)).await;
        };
        assert_ne!(born, first_born, "a fresh incarnation answered");
        assert_eq!(
            id_at, name_at,
            "the restarted id and name are one allocation"
        );
        assert_eq!(name_at, first_name, "a restart reuses the child's name");
    });
}
