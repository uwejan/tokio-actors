//! The options a spawn passes on for the `sys` bundle: they reach the child
//! spec and every restart of a supervised child, state access is allowed
//! only for an actor that opted in and is not sensitive, and the new event
//! kinds sit where the class word expects them.

use std::sync::Arc;
use std::time::Duration;

use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::subtree::{SYS_ALLOW_STATE, SYS_SENSITIVE};
use crate::actor::sys::{DebugOptions, SysError, SysOptions};
use crate::actor::sys_loop::starts;
use crate::actor::sys_state::state_access;
use crate::actor::{Actor, ActorExt, SpawnBuilder};
use crate::error::{ActorError, ActorResult};
use crate::system::ActorSystem;
use crate::telemetry::subscription::{self, Filter, Scope, SubEntry, SubQueue, SubscriptionId};
use crate::telemetry::{ActorRef, EventKind, KindSet, Mode};
use crate::types::ActorId;

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

fn unique(base: &str) -> String {
    format!("sys-options-{base}-{}", uuid::Uuid::new_v4())
}

/// The options every row of the restart test spawns its child with.
fn every_option() -> SysOptions {
    SysOptions {
        debug: DebugOptions::default().statistics(true).log(3).trace(true),
        allow_state_access: true,
        sensitive: true,
    }
}

/// A child that fails when it is told to.
struct Child;

impl Actor for Child {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("told to fail"))
    }
}

/// Spawns a [`Child`] with every option and reports what its spec stored.
struct Parent;

impl Actor for Parent {
    type Message = String;
    type Response = (Option<SysOptions>, ActorHandle<Child>);

    async fn handle(
        &mut self,
        name: String,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<(Option<SysOptions>, ActorHandle<Child>)> {
        let options = every_option();
        let child = ctx
            .spawn_child(|| Child)
            .named(name.clone())
            .debug(options.debug)
            .allow_state_access()
            .sensitive()
            .await?;
        let id = ActorId::from(name.as_str());
        let stored = ctx
            .supervision_ref()
            .and_then(|sup| sup.registry.get(&id))
            .map(|state| state.spec.sys);
        Ok((stored, child))
    }
}

async fn a_restart_spawns_the_next_incarnation_with_the_options_of_the_spec() {
    let system = ActorSystem::create(unique("system")).unwrap();
    let parent = Parent
        .spawn()
        .named(unique("parent"))
        .on_system(&system)
        .supervisor()
        .await
        .unwrap();
    let name = unique("child");
    let (stored, child) = parent.send(name.clone()).await.unwrap();
    assert_eq!(stored, Some(every_option()), "the spec stores the options");

    child.notify(()).await.unwrap();
    let id = ActorId::from(name.as_str());
    tokio::time::timeout(HANG_BOUND, async {
        while starts::of(&id).len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the child was restarted");
    assert_eq!(
        starts::of(&id)[..2],
        [every_option(), every_option()],
        "both incarnations started with the spec's options"
    );
}

#[tokio::test]
async fn restart_keeps_the_options() {
    a_restart_spawns_the_next_incarnation_with_the_options_of_the_spec().await;
}

/// An actor like any other.
#[derive(Default)]
struct Plain;

impl Actor for Plain {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// An actor that is sensitive by its own constant.
#[derive(Default)]
struct Closed;

impl Actor for Closed {
    type Message = ();
    type Response = ();
    const SENSITIVE: bool = true;

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// What a spawn left behind: the verdict on state access the block's bits
/// give, the bits, and the identity the system recorded, with its flag.
async fn left_behind<A: Actor + Default>(
    system: &Arc<ActorSystem>,
    allow: bool,
    sensitive: bool,
) -> (Result<(), SysError>, u8, Option<bool>) {
    let mut spawn: SpawnBuilder<A> = A::default()
        .spawn()
        .named(unique("actor"))
        .on_system(system);
    if allow {
        spawn = spawn.allow_state_access();
    }
    if sensitive {
        spawn = spawn.sensitive();
    }
    let handle = spawn.await.unwrap();
    let record = handle.stop_lane_ref().abort_record();
    let identity = system.telemetry_mode().find(record.key());
    (
        state_access(record),
        record.sys_bits(),
        identity.map(|identity| identity.sensitive()),
    )
}

async fn state_access_is_allowed_only_for_an_opted_in_actor_that_is_not_sensitive() {
    let system = ActorSystem::create(unique("system")).unwrap();
    // (allow, builder sensitive, constant sensitive) -> (verdict, bits, identity)
    let rows = [
        (false, false, false, Err(SysError::NotAllowed), 0, None),
        (true, false, false, Ok(()), SYS_ALLOW_STATE, None),
        (
            false,
            true,
            false,
            Err(SysError::Sensitive),
            SYS_SENSITIVE,
            Some(true),
        ),
        (
            true,
            true,
            false,
            Err(SysError::Sensitive),
            SYS_ALLOW_STATE | SYS_SENSITIVE,
            Some(true),
        ),
        (
            true,
            false,
            true,
            Err(SysError::Sensitive),
            SYS_ALLOW_STATE | SYS_SENSITIVE,
            Some(true),
        ),
    ];
    for (allow, sensitive, constant, verdict, bits, identity) in rows {
        let got = if constant {
            left_behind::<Closed>(&system, allow, sensitive).await
        } else {
            left_behind::<Plain>(&system, allow, sensitive).await
        };
        assert_eq!(
            got,
            (verdict, bits, identity),
            "allow {allow}, builder sensitive {sensitive}, constant sensitive {constant}"
        );
    }
}

#[tokio::test]
async fn state_access_rows() {
    state_access_is_allowed_only_for_an_opted_in_actor_that_is_not_sensitive().await;
}

fn sub_entry(filter: Filter) -> SubEntry {
    let (queue, _rx) = SubQueue::new(4);
    SubEntry {
        id: SubscriptionId(1),
        watcher: ActorRef::new_root(ActorId::from("sys-options"), 1),
        filter,
        queue,
    }
}

#[test]
fn the_new_kinds_sit_at_eighteen_to_twenty_and_trace_is_handler_only() {
    assert_eq!(EventKind::TurnStuck as u8, 18);
    assert_eq!(EventKind::MailboxBacklog as u8, 19);
    assert_eq!(EventKind::Trace as u8, 20);
    assert_eq!(EventKind::LAST, EventKind::Trace);
    assert!(EventKind::Trace.is_hot());
    assert!(!EventKind::TurnStuck.is_hot() && !EventKind::MailboxBacklog.is_hot());
    assert!(KindSet::hot().contains(EventKind::Trace));
    assert!(!KindSet::cold().contains(EventKind::Trace));
    assert!(KindSet::all().contains(EventKind::Trace));

    let mode = Mode::new();
    subscription::register(
        &mode,
        sub_entry(Filter::new(Scope::System).kinds(KindSet::all())),
    );
    assert!(!mode.armed(EventKind::Trace), "a subscription armed Trace");
    assert!(mode.armed(EventKind::TurnStuck) && mode.armed(EventKind::MailboxBacklog));
}
