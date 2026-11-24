use super::*;
use crate::actor::incarnation::SubtreeHold;
use crate::actor::supervision::{
    slot_pair, ChildSpec, ChildState, Predecessor, SupervisionConfig, DEFAULT_KILL_GRACE,
};

struct Idle;

impl Actor for Idle {
    type Message = ();
    type Response = ();

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        Ok(())
    }
}

/// An attempt adopted while its supervisor tears down makes its
/// incarnation the occupant, and the subtree of that incarnation the one a
/// later start for the child waits for.
#[tokio::test]
async fn an_attempt_adopted_in_the_teardown_records_its_subtree() {
    let system = ActorSystem::create(format!("adopted-{}", uuid::Uuid::new_v4())).unwrap();
    let id = ActorId::from("child");
    let (ctl, io) = slot_pair(Default::default());
    let earlier = SubtreeHold::new(None);
    let mut sup = SupervisionState::new(SupervisionConfig::default());
    sup.registry.register(ChildState {
        id: id.clone(),
        name: None,
        spec: ChildSpec {
            restart_type: RestartType::Permanent,
            shutdown: Shutdown::default(),
            start_timeout: None,
            kill_grace: DEFAULT_KILL_GRACE,
            is_supervisor: false,
            significant: false,
            sys: crate::actor::sys::SysOptions::default(),
        },
        ctl,
        lifecycle: ChildLifecycle::Restarting {
            incarnation: 1,
            next: 2,
        },
        origin: Origin::Strategy,
        predecessor: Some(Predecessor::untracked(earlier.watch())),
    });
    let (handle, join, subtree) = spawn_actor(
        id.clone(),
        Idle,
        ActorConfig::default(),
        None,
        Some(system.clone()),
        None,
        false,
        DEFAULT_KILL_GRACE,
        None,
    )
    .expect("the incarnation spawns");
    io.publish(
        handle.stop_lane(),
        handle.status_rx(),
        crate::actor::supervision::OwnStop::worker(DEFAULT_KILL_GRACE),
        Some(subtree),
    );
    let guard = ChildLinkGuard::new(
        handle.stop_lane(),
        join.abort_handle(),
        system.reaper_handle(&Handle::current()),
        crate::actor::supervision::rules::InheritedStop::Kill {
            grace: DEFAULT_KILL_GRACE,
        },
        system.clone(),
        Handle::current(),
    );
    let outcome = RestartOutcome::Adopted {
        child_id: id.clone(),
        incarnation: 2,
        new_stop_lane: handle.stop_lane(),
        new_join: join,
        guard,
        io,
        ended: None,
    };
    let supervisor = Owner::new(ActorId::from("supervisor"), false);
    let (_, _, adopted, _) =
        record_restart_outcome(&mut sup, &supervisor, outcome, false, &mut None);
    assert!(adopted.is_some(), "the attempt is adopted");
    // The earlier incarnation's subtree has ended; the adopted one runs.
    drop(earlier);
    let recorded = sup
        .registry
        .get(&id)
        .and_then(|child| child.predecessor.clone())
        .expect("a predecessor is recorded");
    assert!(
        !recorded.is_gone(),
        "the recorded subtree is the adopted incarnation's, which still runs"
    );
}
