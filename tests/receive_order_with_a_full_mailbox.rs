//! What an actor receives first when its mailbox holds messages: a child's
//! death recorded by its supervisor is received ahead of the messages that
//! child sent before it ended and that are still queued, and, with the
//! mailbox full, ahead of a normal message; a priority message is received
//! ahead of a full mailbox on an actor spawned with a priority lane.
//!
//! The actor under test is held in a turn at a gate while its mailbox fills,
//! so the order it receives in is decided by the run loop alone.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ChildEvent, RestartType, TrySendError,
};

/// Bounds every wait, so a hang fails the test.
const BOUND: Duration = Duration::from_secs(20);

enum Msg {
    /// Signals the first sender once the handler is entered, then holds the
    /// turn until the second receiver completes.
    Block(oneshot::Sender<()>, oneshot::Receiver<()>),
    Tag(&'static str),
}

/// Records what it receives, in order, on a channel the test reads.
struct Receiver {
    seen: mpsc::UnboundedSender<&'static str>,
    child: Arc<Mutex<Option<ActorHandle<Dying>>>>,
    spawns_child: bool,
}

impl Actor for Receiver {
    type Message = Msg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.spawns_child {
            let handle = ctx
                .spawn_child(|| Dying)
                .restart_type(RestartType::Temporary)
                .await?;
            *self.child.lock().unwrap() = Some(handle);
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Block(entered, release) => {
                let _ = entered.send(());
                let _ = release.await;
            }
            Msg::Tag(tag) => {
                let _ = self.seen.send(tag);
            }
        }
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        _event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.seen.send("child_stopped");
        Ok(())
    }
}

/// A child that, on its message, optionally sends one tagged message to its
/// supervisor and then fails.
struct Dying;

impl Actor for Dying {
    type Message = Option<ActorHandle<Receiver>>;
    type Response = ();

    async fn handle(
        &mut self,
        parent: Option<ActorHandle<Receiver>>,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        if let Some(parent) = parent {
            parent
                .notify(Msg::Tag("from-child"))
                .await
                .expect("the parent's mailbox has room");
        }
        Err(ActorError::user("the child ends"))
    }
}

struct Rig {
    receiver: ActorHandle<Receiver>,
    child: Option<ActorHandle<Dying>>,
    seen: mpsc::UnboundedReceiver<&'static str>,
}

async fn start(config: ActorConfig, spawns_child: bool) -> Rig {
    let (seen_tx, seen) = mpsc::unbounded_channel();
    let child = Arc::new(Mutex::new(None));
    let builder = Receiver {
        seen: seen_tx,
        child: child.clone(),
        spawns_child,
    }
    .spawn()
    .with_config(config);
    let builder = if spawns_child {
        builder.supervisor()
    } else {
        builder
    };
    let receiver = builder.await.expect("the receiver starts");
    let child = child.lock().unwrap().take();
    Rig {
        receiver,
        child,
        seen,
    }
}

impl Rig {
    /// Holds the receiver in a turn; the returned sender ends the turn.
    async fn hold_a_turn(&self) -> oneshot::Sender<()> {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        self.receiver
            .notify(Msg::Block(entered_tx, release_rx))
            .await
            .expect("accepted");
        timeout(BOUND, entered_rx)
            .await
            .expect("the turn begins")
            .expect("the handler runs");
        release_tx
    }

    async fn next(&mut self) -> &'static str {
        timeout(BOUND, self.seen.recv())
            .await
            .expect("the receiver gets its next input")
            .expect("the receiver is alive")
    }

    /// Ends the child while the receiver is held in a turn, and returns once
    /// the receiver's runtime has had the child's end to record. On a
    /// single-threaded runtime the watcher of the child's task runs before
    /// this task is polled again, so the yields are margin.
    async fn child_ends(&self, parent: Option<ActorHandle<Receiver>>) {
        let child = self.child.as_ref().expect("the receiver spawned a child");
        child.notify(parent).await.expect("accepted");
        child.wait_stopped().await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }
}

#[test]
fn a_recorded_child_death_is_received_before_the_childs_queued_message() {
    common::run_current_thread_only(async {
        let mut rig = start(ActorConfig::default(), true).await;
        let release = rig.hold_a_turn().await;
        rig.child_ends(Some(rig.receiver.clone())).await;
        assert_eq!(
            rig.receiver.mailbox_len(),
            1,
            "the message the child sent before it ended is still queued"
        );
        release.send(()).unwrap();
        assert_eq!(rig.next().await, "child_stopped");
        assert_eq!(rig.next().await, "from-child");
    });
}

#[test]
fn a_priority_message_is_received_before_a_message_in_a_full_mailbox() {
    common::run(2, async {
        let mut rig = start(
            ActorConfig::default()
                .with_mailbox_capacity(1)
                .with_priority_lane(1),
            false,
        )
        .await;
        let release = rig.hold_a_turn().await;
        rig.receiver
            .try_notify(Msg::Tag("normal"))
            .expect("room for one message");
        assert!(
            matches!(
                rig.receiver.try_notify(Msg::Tag("refused")),
                Err(TrySendError::Full)
            ),
            "the mailbox is full"
        );
        rig.receiver
            .try_notify_priority(Msg::Tag("priority"))
            .expect("the priority lane has room");
        release.send(()).unwrap();
        assert_eq!(rig.next().await, "priority");
        assert_eq!(rig.next().await, "normal");
    });
}
