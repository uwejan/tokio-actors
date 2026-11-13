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
use crate::actor::Actor;
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
