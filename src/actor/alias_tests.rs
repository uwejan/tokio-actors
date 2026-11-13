//! The guard of an alias, ordered against `unalias` with a hook between a
//! send's check and its put.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::actor::alias::{AliasOptions, AliasShared};
use crate::types::{ActorId, Incarnation};

fn shared(options: AliasOptions) -> Arc<AliasShared> {
    Arc::new(AliasShared::new(
        options,
        (ActorId::from("owner"), Incarnation::mint()),
    ))
}

/// A send that has passed its check and not yet put its message holds the
/// read side of the guard: `unalias` does not return until the put is done, and
/// a send that begins after `unalias` returned finds the alias ended.
#[test]
fn guard_orders_a_send_against_unalias() {
    let alias = shared(AliasOptions::default());
    let (inside_tx, inside_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    *alias.between.lock().unwrap() = Some(Arc::new(move || {
        inside_tx.send(()).unwrap();
        go_rx.lock().unwrap().recv().unwrap();
    }));
    let put = Arc::new(AtomicBool::new(false));

    let sender = {
        let (alias, put) = (alias.clone(), put.clone());
        std::thread::spawn(move || alias.enter(|| put.store(true, Ordering::SeqCst)))
    };
    inside_rx.recv().unwrap();

    let (done_tx, done_rx) = mpsc::channel();
    let owner = {
        let (alias, put) = (alias.clone(), put.clone());
        std::thread::spawn(move || {
            let was_active = alias.deactivate();
            let put_first = put.load(Ordering::SeqCst);
            done_tx.send((was_active, put_first)).unwrap();
        })
    };
    // The write side waits for the sender that holds the read side.
    assert!(
        done_rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "unalias returned while a send was between its check and its put"
    );
    go_tx.send(()).unwrap();
    assert_eq!(sender.join().unwrap(), Some(()));
    owner.join().unwrap();
    assert_eq!(done_rx.recv().unwrap(), (true, true));

    *alias.between.lock().unwrap() = None;
    assert_eq!(alias.enter(|| ()), None, "a send after unalias is refused");
}

/// The owner's end waits on no sender: a send that sits between its check and
/// its put, holding the read side of the guard, does not hold the owner's end
/// back, so a message whose drop is slow cannot delay the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_owners_end_does_not_wait_for_a_send_in_its_guard() {
    use crate::actor::alias::Alias;
    use crate::actor::context::ActorContext;
    use crate::actor::{Actor, ActorExt};
    use crate::error::ActorResult;
    use crate::types::StopReason;
    use tokio::sync::oneshot;

    struct Owner;
    impl Actor for Owner {
        type Message = oneshot::Sender<Alias<Owner>>;
        type Response = ();
        async fn handle(
            &mut self,
            reply: Self::Message,
            ctx: &mut ActorContext<Self>,
        ) -> ActorResult<()> {
            let _ = reply.send(ctx.alias(AliasOptions::default()));
            Ok(())
        }
    }

    let owner = Owner.spawn().await.unwrap();
    let (tx, rx) = oneshot::channel();
    owner.notify(tx).await.unwrap();
    let alias = rx.await.unwrap();
    let (inside_tx, inside_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    *alias.shared().between.lock().unwrap() = Some(Arc::new(move || {
        inside_tx.send(()).unwrap();
        go_rx.lock().unwrap().recv().unwrap();
    }));
    let sender = {
        let alias = alias.clone();
        tokio::spawn(async move {
            let (tx, _rx) = oneshot::channel();
            alias.notify(tx).await
        })
    };
    inside_rx.recv().unwrap();
    owner.stop(StopReason::Graceful).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(10), owner.wait_stopped()).await;
    *alias.shared().between.lock().unwrap() = None;
    go_tx.send(()).unwrap();
    let _ = sender.await;
    assert!(
        ended.is_ok(),
        "the owner's end waited for a send in its guard"
    );
    assert!(!alias.is_active());
}
