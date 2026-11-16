//! Supervised event handlers: the owner's handlers leave with it, and the owner
//! hears once when a handler leaves, whichever way it leaves.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use tokio_actors::event::{
    BoxedHandler, EventError, EventHandler, EventManager, EventManagerRef, Handled, HandlerExit,
    HandlerId, Removed,
};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, StopReason,
};

type Mgr = EventManagerRef<u8, (), ()>;
/// Leaves on its first event when `leave` is set, and reports each `terminate`.
struct Probe {
    leave: bool,
    log: mpsc::UnboundedSender<String>,
}

impl EventHandler for Probe {
    type Event = u8;
    type Call = ();
    type Reply = ();

    fn handle_event(&mut self, _: &u8) -> Handled {
        if self.leave {
            Handled::Remove
        } else {
            Handled::Keep
        }
    }

    fn terminate(&mut self, why: &Removed) -> Option<()> {
        let _ = self.log.send(format!("{why:?}"));
        None
    }
}

/// Holds the manager inside its turn until the test releases it.
struct Gate {
    inside: mpsc::UnboundedSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

impl EventHandler for Gate {
    type Event = u8;
    type Call = ();
    type Reply = ();

    fn handle_event(&mut self, _: &u8) -> Handled {
        let _ = self.inside.send(());
        let _ = self.release.recv();
        Handled::Keep
    }
}

enum Msg {
    Add(Mgr, Probe, oneshot::Sender<Result<HandlerId, EventError>>),
    /// Polls the add once and drops it; answers whether it was still waiting.
    AddDropped(Mgr, Probe, oneshot::Sender<bool>),
    Exit(HandlerExit),
}

struct Owner(mpsc::UnboundedSender<HandlerExit>);
impl Actor for Owner {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            Msg::Add(mgr, probe, back) => {
                let _ = back.send(ctx.add_sup_handler(&mgr, probe, Msg::Exit).await);
            }
            Msg::AddDropped(mgr, probe, back) => {
                let waiting = tokio::select! {
                    biased;
                    _ = ctx.add_sup_handler(&mgr, probe, Msg::Exit) => false,
                    () = std::future::ready(()) => true,
                };
                let _ = back.send(waiting);
            }
            Msg::Exit(exit) => {
                let _ = self.0.send(exit);
            }
        }
        Ok(())
    }
}

async fn seen<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    timeout(Duration::from_secs(30), rx.recv())
        .await
        .expect("a hang")
        .expect("the sender is alive")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_supervised() {
    let rows = [
        "leaves",
        "owner ends",
        "manager ends",
        "swap",
        "swap panics",
        "manager killed",
    ];
    for row in rows {
        let manager = EventManager::<u8>::new().spawn().await.unwrap();
        let mgr = EventManagerRef::of(&manager);
        let (exits, mut exited) = mpsc::unbounded_channel();
        let (log, mut logged) = mpsc::unbounded_channel();
        let owner = Owner(exits).spawn().await.unwrap();
        let (back, id) = oneshot::channel();
        let probe = |leave| Probe {
            leave,
            log: log.clone(),
        };
        owner
            .notify(Msg::Add(mgr.clone(), probe(row == "leaves"), back))
            .await
            .unwrap();
        let id = id.await.unwrap().unwrap();
        let exit = |reason| HandlerExit { id, reason };
        assert_eq!(mgr.which_handlers().await.unwrap(), [id], "{row}");

        let reason = match row {
            "leaves" => {
                assert_eq!(mgr.sync_notify(1).await.unwrap(), [exit(Removed::Returned)]);
                Removed::Returned
            }
            "owner ends" => {
                owner.stop(StopReason::Graceful).await.unwrap();
                assert_eq!(seen(&mut logged).await, "OwnerEnded(Graceful)");
                assert!(mgr.which_handlers().await.unwrap().is_empty());
                continue;
            }
            "manager ends" => {
                manager.stop(StopReason::Graceful).await.unwrap();
                Removed::Stop(StopReason::Graceful)
            }
            "manager killed" => {
                manager.stop(StopReason::Kill).await.unwrap();
                Removed::Stop(StopReason::Kill)
            }
            "swap" => {
                let new: BoxedHandler<u8, (), ()> = Box::new(probe(true));
                mgr.swap_handler(id, move |_| new).await.unwrap();
                mgr.sync_notify(1).await.unwrap();
                Removed::Returned
            }
            _ => {
                let err = mgr.swap_handler(id, |_| panic!("no handler")).await;
                assert_eq!(err, Err(EventError::HandlerPanicked("no handler".into())));
                Removed::Panicked("no handler".into())
            }
        };
        // The first notice is the removal itself (a swap sent none), and the
        // owner's end closes the channel after it: there was only one.
        assert_eq!(seen(&mut exited).await, exit(reason), "{row}");
        if row == "manager killed" {
            assert!(
                logged.try_recv().is_err(),
                "a killed manager runs no terminate"
            );
        }
        owner.stop(StopReason::Graceful).await.unwrap();
        assert!(exited.recv().await.is_none(), "{row}: one notice per slot");
    }

    // A bound that passes while the add is still queued, then a manager that
    // ends Graceful with the add unserved: the owner hears once, with the
    // reason the manager ended with.
    let manager = EventManager::<u8>::new().spawn().await.unwrap();
    let mgr = EventManagerRef::of(&manager);
    let (exits, mut exited) = mpsc::unbounded_channel();
    let owner = Owner(exits).spawn().await.unwrap();
    let (inside, mut entered) = mpsc::unbounded_channel();
    let (release, gate) = std::sync::mpsc::channel();
    let gate = Gate {
        inside,
        release: gate,
    };
    mgr.add_handler(gate).await.unwrap();
    mgr.notify(1).await.unwrap();
    seen(&mut entered).await;
    let (back, added) = oneshot::channel();
    let (log, _logged) = mpsc::unbounded_channel();
    let bounded = mgr.clone().timeout(Duration::from_millis(50));
    let probe = Probe { leave: false, log };
    owner.notify(Msg::Add(bounded, probe, back)).await.unwrap();
    assert_eq!(added.await.unwrap(), Err(EventError::Timeout));
    manager.stop(StopReason::Graceful).await.unwrap();
    release.send(()).unwrap();
    let exit = seen(&mut exited).await;
    assert_eq!(exit.reason, Removed::Stop(StopReason::Graceful));
    assert_eq!(manager.wait_stopped().await, StopReason::Graceful);
    owner.stop(StopReason::Graceful).await.unwrap();
    assert!(
        exited.recv().await.is_none(),
        "one notice for the queued add"
    );

    // An add whose future is dropped while it waits for room in the manager's
    // full mailbox: the add was never queued, so no handler exists and the
    // owner hears nothing, not even at its own stop, which delivers every
    // notice still owed.
    let manager = EventManager::<u8>::new().spawn().await.unwrap();
    let mgr = EventManagerRef::of(&manager);
    let (exits, mut exited) = mpsc::unbounded_channel();
    let owner = Owner(exits).spawn().await.unwrap();
    let (inside, mut entered) = mpsc::unbounded_channel();
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = Gate {
        inside,
        release: gate,
    };
    let gate = mgr.add_handler(gate).await.unwrap();
    mgr.notify(1).await.unwrap();
    seen(&mut entered).await;
    while mgr.try_notify(1).is_ok() {}
    assert_eq!(mgr.try_notify(1), Err(EventError::Full));
    let (back, waited) = oneshot::channel();
    let (log, _logged) = mpsc::unbounded_channel();
    let probe = Probe { leave: false, log };
    owner
        .notify(Msg::AddDropped(mgr.clone(), probe, back))
        .await
        .unwrap();
    assert!(waited.await.unwrap(), "the add waited for room");
    drop(release);
    assert_eq!(mgr.which_handlers().await.unwrap(), [gate]);
    owner.stop(StopReason::Graceful).await.unwrap();
    assert!(
        exited.recv().await.is_none(),
        "no notice for an add never queued"
    );
}
