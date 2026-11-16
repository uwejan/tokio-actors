//! A registration refused because its id, which is a named actor's name, is
//! already registered leaves nothing behind, and the registration that holds
//! the id is untouched. The public spawn paths refuse a held name before they
//! reach the registry, so these tests drive the registry's own check with
//! handles built by hand.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::incarnation::StatusValue;
use crate::actor::incarnation::{StatusTx, StopLane, TurnClock};
use crate::actor::Actor;
use crate::error::{ActorResult, SpawnError};
use crate::system::ActorSystem;
use crate::types::{ActorId, ActorStatus};

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
    _lane: crate::actor::incarnation::LaneRx,
}

fn handle_for(id: &ActorId) -> (ActorHandle<Idle>, Keep) {
    let (tx, mailbox) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (lane, lane_rx) = StopLane::new();
    let handle = ActorHandle::<Idle>::new(
        id.clone(),
        tx,
        lane,
        1,
        status_rx,
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
    named: bool,
    handle: &ActorHandle<Idle>,
) -> Result<u64, SpawnError> {
    sys.register_actor::<Idle>(
        id,
        named,
        handle,
        false,
        TurnClock::default(),
        Duration::from_millis(100),
        Handle::current(),
    )
}
