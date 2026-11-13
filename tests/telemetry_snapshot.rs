//! `ActorSystem::snapshot`: a read of the registry that asks no actor.
//!
//! A stuck actor is listed with its queue while `get_status` cannot be
//! answered, the ancestry names the incarnations that spawned each actor, a
//! label, the type name and the child count are reported, and a storm of
//! spawns and stops beside the snapshot neither deadlocks it nor leaves a dead
//! actor in it.

mod common;

use std::future::IntoFuture;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{sleep, timeout, Instant};
use tokio_actors::telemetry::{ActorRecord, EventKind, KindSet};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorResult, ActorStatus, ActorSystem, SystemConfig,
};

const BOUND: Duration = Duration::from_secs(30);

/// How long a snapshot may take: a read of the registry that asks no actor.
const SNAPSHOT_BOUND: Duration = Duration::from_secs(5);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "telemetry-snapshot-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

async fn within<F: IntoFuture>(what: &str, fut: F) -> F::Output {
    match timeout(BOUND, fut.into_future()).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {BOUND:?} waiting for: {what}"),
    }
}

/// Polls `pred` until it holds, at most `BOUND`.
async fn until(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + BOUND;
    while !pred() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        sleep(Duration::from_millis(2)).await;
    }
}

/// A system with the tracking switch as the row says, and a watchdog
/// threshold far above anything a test waits for when tracking is on, so a
/// turn in flight is timed whichever switch is asked to time it.
fn system(base: &str, tracking: bool) -> Arc<ActorSystem> {
    system_with(base, tracking, tracking)
}

/// A system with turn tracking and the watchdog threshold as the row says.
fn system_with(base: &str, tracking: bool, threshold: bool) -> Arc<ActorSystem> {
    let mut config = SystemConfig::default();
    if tracking {
        config = config.with_turn_tracking();
    }
    if threshold {
        config = config.with_turn_warning_threshold(Duration::from_secs(3600));
    }
    ActorSystem::create_with(uname(base), config).unwrap()
}

fn find<'a>(records: &'a [ActorRecord], name: &str) -> Option<&'a ActorRecord> {
    records
        .iter()
        .find(|record| record.name.as_deref() == Some(name))
}

// ---------------------------------------------------------------------------
// A stuck actor
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Gate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

/// Lets go of its gate when dropped, so a failed assertion never leaves a
/// handler waiting.
struct Release(Arc<Notify>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

enum HeldMsg {
    Hold,
    Plain,
}

/// Holds its turn on a gate while it handles `Hold`.
struct Held(Gate);

impl Actor for Held {
    type Message = HeldMsg;
    type Response = ();

    async fn handle(&mut self, msg: HeldMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let HeldMsg::Hold = msg {
            self.0.entered.notify_one();
            self.0.release.notified().await;
        }
        Ok(())
    }
}

#[test]
fn a_snapshot_lists_a_stuck_actor_that_get_status_cannot_reach() {
    // The row is whether turns are timed, by turn tracking or by a watchdog
    // threshold: `in_turn` reads `Some` only then.
    for (tracking, threshold) in [(false, false), (true, false), (false, true)] {
        common::run_or_leak(2, async move {
            let sys = system_with("stuck", tracking, threshold);
            let gate = Gate::default();
            let name = uname("held");
            let held = Held(gate.clone())
                .spawn()
                .named(name.clone())
                .on_system(&sys)
                .await
                .unwrap();
            let _release = Release(gate.release.clone());

            held.notify(HeldMsg::Hold).await.unwrap();
            within("the handler to hold its turn", gate.entered.notified()).await;
            held.notify(HeldMsg::Plain).await.unwrap();

            // The status request waits for a turn boundary the actor does not
            // reach, so it is still pending when the snapshot has answered.
            let status = held.get_status();
            tokio::pin!(status);
            assert!(
                timeout(Duration::from_millis(50), &mut status)
                    .await
                    .is_err(),
                "get_status is served only at a turn boundary"
            );

            // On its own thread: a snapshot that waited for the actor would
            // not finish before the gate opens, which is after it returns. The
            // bound is far above what a read of the registry takes. A
            // snapshot stuck in the registry also holds up the runtime's
            // drop, so the body runs under `run_or_leak`: the bound then
            // fails the test at once.
            let reader = sys.clone();
            let records = timeout(
                SNAPSHOT_BOUND,
                tokio::task::spawn_blocking(move || reader.snapshot()),
            )
            .await
            .expect("the snapshot returns while the handler holds its turn")
            .unwrap();

            let record = find(&records, &name).expect("the stuck actor is listed");
            assert_eq!(record.status, ActorStatus::Running);
            assert_eq!(
                record.mailbox_len, 1,
                "the queued message, not the one being handled"
            );
            assert_eq!(
                record.in_turn.is_some(),
                tracking || threshold,
                "tracking {tracking}, threshold {threshold}"
            );
            assert!(record.type_name.ends_with("Held"), "{}", record.type_name);
            assert_eq!(record.actor.id(), held.id());
            assert_eq!(record.mailbox_capacity, held.mailbox_capacity());
            assert_eq!(record.priority_len, 0);
            assert_eq!(record.child_count, 0);
            assert_eq!(record.label, None);
            assert!(record.waiting_on.is_none());
            assert_eq!(records.len(), 1);

            gate.release.notify_one();
            let info = within("the status request", &mut status).await.unwrap();
            assert_eq!(info.id.as_str(), name);
        });
    }
}

// ---------------------------------------------------------------------------
// Ancestry, label, type name, child count
// ---------------------------------------------------------------------------

enum NodeMsg {
    Crash,
    Add(String),
    Remove(String),
}

/// A supervision tree that builds itself: a node with `below` levels under it
/// spawns one named node with one level less, and a labelled sibling when the
/// root asks for one.
struct Node {
    base: String,
    below: usize,
    label: Option<&'static str>,
    sibling: bool,
}

impl Node {
    fn leaf(label: Option<&'static str>) -> Self {
        Node {
            base: String::new(),
            below: 0,
            label,
            sibling: false,
        }
    }
}

impl Actor for Node {
    type Message = NodeMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(label) = self.label {
            ctx.set_label(label);
        }
        if self.below > 0 {
            let (base, below) = (self.base.clone(), self.below - 1);
            let name = format!("{}-{}", self.base, below);
            ctx.spawn_child(move || Node {
                base: base.clone(),
                below,
                label: None,
                sibling: false,
            })
            .named(name)
            .with_config(ActorConfig::default().supervisor())
            .await?;
        }
        if self.sibling {
            ctx.spawn_child(|| Node::leaf(Some("sibling")))
                .named(format!("{}-sibling", self.base))
                .await?;
        }
        Ok(())
    }

    async fn handle(&mut self, msg: NodeMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            NodeMsg::Crash => return Err(ActorError::user("crash on command")),
            NodeMsg::Add(name) => {
                ctx.spawn_child(|| Node::leaf(None)).named(name).await?;
            }
            NodeMsg::Remove(name) => {
                ctx.terminate_child(name.clone()).await?;
                ctx.delete_child(name)?;
            }
        }
        Ok(())
    }
}

#[test]
fn a_snapshot_names_the_incarnations_that_spawned_each_actor() {
    // The row is whether a sink took events when the tree was spawned, which
    // decides whether each actor's identity was recorded at its spawn or is
    // rebuilt from the registry when the snapshot asks.
    for sink in [false, true] {
        common::run(2, async move {
            let sys = system("tree", false);
            if sink {
                assert!(sys.attach_telemetry(
                    "tree",
                    KindSet::of(&[EventKind::ActorEnded]),
                    |_| {}
                ));
            }
            let base = uname("n");
            let (root, mid, leaf, sibling) = (
                format!("{base}-root"),
                format!("{base}-1"),
                format!("{base}-0"),
                format!("{base}-sibling"),
            );
            let tree = Node {
                base: base.clone(),
                below: 2,
                label: None,
                sibling: true,
            }
            .spawn()
            .named(root.clone())
            .on_system(&sys)
            .with_config(ActorConfig::default().supervisor())
            .await
            .unwrap();
            // A second system's actor is never listed in this one.
            let other = system("other", false);
            let _stranger = Node::leaf(None)
                .spawn()
                .named(uname("stranger"))
                .on_system(&other)
                .await
                .unwrap();

            // Four actors are listed, and the three child specs they hold
            // (two under the root, one under the next level) are recorded.
            until("the tree to be up", || {
                let records = sys.snapshot();
                records.len() == 4 && records.iter().map(|r| r.child_count).sum::<usize>() == 3
            })
            .await;
            let records = sys.snapshot();
            assert!(!records
                .iter()
                .any(|r| r.name.as_deref().is_some_and(|n| n.contains("stranger"))));

            // The tree: <base>-root, then <base>-1 under it and <base>-0 under
            // that, and the labelled sibling under the root.
            let node = |name: &str| find(&records, name).unwrap_or_else(|| panic!("{name} listed"));
            assert_eq!(node(&root).actor.id(), tree.id());
            assert!(node(&root).type_name.ends_with("Node"));
            assert_eq!(node(&sibling).label.as_deref(), Some("sibling"));
            assert_eq!(node(&root).label, None);
            assert_eq!(node(&root).child_count, 2, "the next level and the sibling");
            assert_eq!(node(&mid).child_count, 1);
            assert_eq!(node(&leaf).child_count, 0);

            // Ancestors, parent first, with the incarnations of the actors
            // that spawned it.
            let chain = |name: &str| -> Vec<(String, u64)> {
                node(name)
                    .actor
                    .ancestors()
                    .map(|(id, incarnation)| (id.as_str().to_string(), incarnation))
                    .collect()
            };
            let (r, m) = (
                node(&root).actor.incarnation(),
                node(&mid).actor.incarnation(),
            );
            assert_eq!(chain(&root), vec![]);
            assert_eq!(chain(&mid), vec![(root.clone(), r)]);
            assert_eq!(chain(&leaf), vec![(mid.clone(), m), (root.clone(), r)]);
            assert_eq!(chain(&sibling), vec![(root.clone(), r)]);

            // A restart of the middle node: its child is spawned again under
            // the new incarnation, and the ancestry says so.
            let mid_handle = sys
                .get::<Node>(&mid)
                .expect("the middle node is registered");
            mid_handle.notify(NodeMsg::Crash).await.unwrap();
            until("the middle node and its child to be running again", || {
                let records = sys.snapshot();
                match (find(&records, &mid), find(&records, &leaf)) {
                    (Some(mid_now), Some(leaf_now)) => {
                        mid_now.actor.incarnation() > m
                            && leaf_now.actor.ancestors().next().map(|(_, n)| n)
                                == Some(mid_now.actor.incarnation())
                    }
                    _ => false,
                }
            })
            .await;
            let records = sys.snapshot();
            let node = |name: &str| find(&records, name).unwrap();
            let m2 = node(&mid).actor.incarnation();
            assert_ne!(m2, m);
            assert_eq!(
                node(&leaf)
                    .actor
                    .ancestors()
                    .map(|(id, incarnation)| (id.as_str().to_string(), incarnation))
                    .collect::<Vec<_>>(),
                vec![(mid.clone(), m2), (root.clone(), r)],
                "the child names the incarnation of the middle node that spawned it"
            );

            // The child count follows an add and a remove.
            let counted = |sys: &ActorSystem| {
                let records = sys.snapshot();
                find(&records, &root).map(|r| r.child_count)
            };
            assert_eq!(counted(&sys), Some(2));
            tree.send(NodeMsg::Add(format!("{base}-extra")))
                .await
                .unwrap();
            assert_eq!(counted(&sys), Some(3));
            tree.send(NodeMsg::Remove(format!("{base}-extra")))
                .await
                .unwrap();
            assert_eq!(counted(&sys), Some(2));
        });
    }
}

// ---------------------------------------------------------------------------
// Spawns and stops beside the snapshot
// ---------------------------------------------------------------------------
