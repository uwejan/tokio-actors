//! A registration refused because its id is already registered leaves nothing
//! behind: the name it claimed an instant earlier is given back, and the
//! registration that holds the id is untouched. The public spawn paths refuse
//! a duplicate id before they reach the registry, so these tests drive the
//! registry's own check with handles built by hand.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorResult, SpawnError};
use crate::system::ActorSystem;
use crate::types::{ActorId, ActorStatus, StatusTx, StatusValue, StopLane, TurnClock};

#[derive(Default)]
struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// What keeps a hand-built handle's channels open.
struct Keep {
    _mailbox: crate::actor::mailbox::MailRx<crate::actor::ActorEnvelope<Idle>>,
    _status: StatusTx,
    _lane: crate::types::LaneRx,
}

fn handle_for(sys: &ActorSystem, id: &ActorId) -> (ActorHandle<Idle>, Keep) {
    let (tx, mailbox) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (lane, lane_rx) = StopLane::new();
    let handle = ActorHandle::<Idle>::new(
        id.clone(),
        tx,
        lane,
        1,
        status_rx,
        sys.name().into(),
        None,
        Some(Arc::new(AtomicBool::new(false))),
    );
    (
        handle,
        Keep {
            _mailbox: mailbox,
            _status: status_tx,
            _lane: lane_rx,
        },
    )
}

fn register(
    sys: &ActorSystem,
    id: &ActorId,
    name: Option<&str>,
    handle: &ActorHandle<Idle>,
) -> Result<u64, SpawnError> {
    let name: Option<Arc<str>> = name.map(Arc::from);
    sys.register_actor::<Idle>(
        id,
        name.as_ref(),
        handle,
        false,
        TurnClock::default(),
        Duration::from_millis(100),
        Handle::current(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_duplicate_id_gives_back_the_name_it_claimed() {
    let sys = ActorSystem::create(format!("duplicate-id-{}", uuid::Uuid::new_v4())).unwrap();
    let held = ActorId::from("duplicate-id-held");
    let (holder, _keep_holder) = handle_for(&sys, &held);
    register(&sys, &held, None, &holder).expect("the first registration of the id");

    // The same id again, under a name nothing holds: refused on the id.
    let (second, _keep_second) = handle_for(&sys, &held);
    let refused = register(&sys, &held, Some("claimed-then-given-back"), &second);
    assert_eq!(refused, Err(SpawnError::DuplicateChild(held.clone())));

    assert!(
        !sys.registered()
            .contains(&"claimed-then-given-back".to_string()),
        "the refused registration left its name registered"
    );
    assert!(sys.get::<Idle>("claimed-then-given-back").is_none());
    assert!(
        sys.get_by_id::<Idle>(&held).is_some(),
        "the registration that holds the id is untouched"
    );

    // The name can be registered again, for another id and by a public spawn.
    let other = ActorId::from("duplicate-id-other");
    let (third, _keep_third) = handle_for(&sys, &other);
    register(&sys, &other, Some("claimed-then-given-back"), &third)
        .expect("the name registers again after the refusal");
    assert!(sys.get::<Idle>("claimed-then-given-back").is_some());

    let spawned = Idle
        .spawn()
        .named("also-free")
        .on_system(&sys)
        .await
        .expect("a public spawn under a fresh name still works");
    assert!(sys.actor_ids().contains(spawned.id()));
}
