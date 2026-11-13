//! The gate, the identity of an incarnation and its ancestry.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{ActorRef, EventKind, KindSet, TelemetryEvent};
use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::runtime::ActorConfig;
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorError, ActorResult};
use crate::system::ActorSystem;
use crate::types::{ActorId, Shutdown, StopReason};

/// The longest a wait for a condition may take; it only limits how long a
/// real hang takes to report.
const BOUND: Duration = Duration::from_secs(60);

const KINDS: [EventKind; 8] = [
    EventKind::StopRequested,
    EventKind::ActorEnded,
    EventKind::ChildDied,
    EventKind::RestartBudget,
    EventKind::Unresponsive,
    EventKind::KillStep,
    EventKind::HelperEnded,
    EventKind::HandlerDetached,
];

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

fn unique(base: &str) -> String {
    format!(
        "gate-{base}-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

fn event() -> TelemetryEvent<'static> {
    TelemetryEvent::HandlerDetached {
        handler: "h".into(),
        panic: "p".into(),
    }
}

/// Runs `body` on a multi-thread runtime and on a current-thread runtime.
fn both_flavors<F, Fut>(body: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for multi in [true, false] {
        let runtime = if multi {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        };
        runtime.block_on(body());
    }
}

/// Waits until `done` holds, or fails after the bound.
async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + BOUND;
    while !done() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// The gate: a site builds only when a sink of its kind is armed on its system
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Setup {
    NoSink,
    SinkForAnotherKind,
    SinkForThisKind,
    SinkForThisKindOnAnotherSystem,
}

/// Each setup and how many times a site of the kind builds under it.
const SETUPS: [(Setup, usize); 4] = [
    (Setup::NoSink, 0),
    (Setup::SinkForAnotherKind, 0),
    (Setup::SinkForThisKind, 1),
    (Setup::SinkForThisKindOnAnotherSystem, 0),
];

// ---------------------------------------------------------------------------
// Identity and ancestry from real spawns
// ---------------------------------------------------------------------------

type Seen = Arc<Mutex<Vec<ActorRef>>>;
type Handles = Arc<Mutex<Vec<ActorHandle<Node>>>>;

/// A supervisor of one child (and, optionally, one more leaf) that publishes
/// its own reference and handle when it starts, so a test can read the
/// ancestry every incarnation was spawned with. A message crashes it.
struct Node {
    depth: u8,
    sibling: bool,
    seen: Seen,
    handles: Handles,
}

impl Node {
    fn below(&self, depth: u8) -> Node {
        Node {
            depth,
            sibling: false,
            seen: self.seen.clone(),
            handles: self.handles.clone(),
        }
    }
}

impl Actor for Node {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let me = super::identity_of(&ctx.resolved_system(), ctx.stop_lane().abort_record())
            .map_or_else(
                || ActorRef::new_root(ctx.actor_id().clone(), super::UNKNOWN_INCARNATION),
                |identity| identity.actor().clone(),
            );
        self.seen.lock().unwrap().push(me);
        self.handles.lock().unwrap().push(ctx.self_handle());
        if self.sibling {
            let template = self.below(0);
            ctx.spawn_child(move || template.below(0)).await?;
        }
        if self.depth > 0 {
            let depth = self.depth - 1;
            let template = self.below(depth);
            let mut child = ctx.spawn_child(move || template.below(depth));
            if depth > 0 {
                child = child
                    .with_config(ActorConfig::default().supervisor())
                    .shutdown(Shutdown::Infinity);
            }
            child.await?;
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("crash"))
    }
}

fn pairs(actor: &ActorRef) -> Vec<(ActorId, u64)> {
    actor
        .ancestors()
        .map(|(id, incarnation)| (id.clone(), incarnation))
        .collect()
}

fn named(actor: &ActorRef) -> (ActorId, u64) {
    (actor.id().clone(), actor.incarnation())
}

fn root_node(depth: u8, sibling: bool) -> (Seen, Handles, Node) {
    let seen: Seen = Arc::default();
    let handles: Handles = Arc::default();
    let root = Node {
        depth,
        sibling,
        seen: seen.clone(),
        handles: handles.clone(),
    };
    (seen, handles, root)
}

async fn start_tree(
    sys: &Arc<ActorSystem>,
    depth: u8,
    sibling: bool,
) -> (Seen, Handles, ActorHandle<Node>) {
    let (seen, handles, root) = root_node(depth, sibling);
    let handle = root.spawn().on_system(sys).supervisor().await.unwrap();
    (seen, handles, handle)
}

fn actor_ref_of(sys: &ActorSystem, handle: &ActorHandle<Node>) -> ActorRef {
    super::identity_of(sys, handle.stop_lane_ref().abort_record())
        .expect("the identity is recorded")
        .actor()
        .clone()
}

#[test]
fn a_child_names_its_ancestors_as_they_were_when_it_was_spawned() {
    both_flavors(|| async {
        let sys = ActorSystem::create(unique("ancestry")).unwrap();
        let _sink = sys.telemetry_mode().collect(KindSet::all());
        // root -> a -> b -> leaf, and a sibling leaf of `a` under the root.
        let (seen, _handles, root) = start_tree(&sys, 3, true).await;
        // A supervised child's first start is not awaited by its spawner.
        until("the tree to start", || seen.lock().unwrap().len() == 5).await;
        // The children start at once, so the order they published in is not
        // fixed; their numbers follow the order they were spawned in.
        let mut refs = seen.lock().unwrap().clone();
        refs.sort_by_key(ActorRef::incarnation);
        let (r, sibling, a, b, leaf) = (&refs[0], &refs[1], &refs[2], &refs[3], &refs[4]);
        assert_eq!(r.id(), root.id());

        let incarnations: std::collections::HashSet<u64> =
            refs.iter().map(ActorRef::incarnation).collect();
        assert_eq!(incarnations.len(), 5, "no two incarnations share a number");

        assert!(pairs(r).is_empty(), "a root has no ancestors");
        assert_eq!(pairs(sibling), vec![named(r)]);
        assert_eq!(pairs(a), vec![named(r)]);
        assert_eq!(pairs(b), vec![named(a), named(r)]);
        assert_eq!(pairs(leaf), vec![named(b), named(a), named(r)]);

        for up in [b, a, r] {
            assert!(leaf.is_descendant_of(up.id()));
        }
        assert!(
            !sibling.is_descendant_of(a.id()),
            "a sibling is not below its sibling"
        );
        assert!(
            !a.is_descendant_of(leaf.id()),
            "an ancestor is not below its descendant"
        );
        assert!(
            !leaf.is_descendant_of(leaf.id()),
            "an actor is not below itself"
        );
        assert!(!leaf.is_descendant_of(&ActorId::from("stranger")));

        // The references answer after the whole tree has ended: the chain is
        // in the reference, not looked up in the registry.
        let before: Vec<_> = refs.iter().map(pairs).collect();
        root.stop(StopReason::Graceful).await.unwrap();
        root.wait_stopped().await;
        until("the tree to leave the registry", || {
            sys.get_by_id::<Node>(leaf.id()).is_none()
        })
        .await;
        let after: Vec<_> = refs.iter().map(pairs).collect();
        assert_eq!(before, after);
        assert!(leaf.is_descendant_of(r.id()));
    });
}
