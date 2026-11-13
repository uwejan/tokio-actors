#![allow(dead_code)]
//! `StopReason::Shutdown` is a clean, vetoable request that ranks above
//! `Graceful` and below `ParentRequest`: it never replaces a pending request
//! of a stronger reason, an outside `Graceful` never replaces it, it passes
//! `pre_stop` like a graceful stop, and a `Transient` child that ends with
//! it is not restarted (Erlang/OTP `{shutdown, Term}`).
//!
//! Nothing is ordered by a sleep: the actors park at gates the tests open,
//! and every wait is bounded only to report a hang.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorHandle, ActorResult, ChildEvent, RestartType, StopReason,
};

mod common;

/// The longest a wait may take. It only limits how long a real hang takes to
/// report.
const BOUND: Duration = Duration::from_secs(30);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "shutdown-reason-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn shutdown() -> StopReason {
    StopReason::Shutdown("done".into())
}

fn failure() -> StopReason {
    StopReason::Failure(ActorError::user("f"))
}

/// Every variant, from one match that fails to compile when a variant is
/// added: a new reason has to be classified by every table below.
fn every_reason() -> Vec<StopReason> {
    let all = vec![
        StopReason::Graceful,
        StopReason::ParentRequest,
        shutdown(),
        failure(),
        StopReason::ParentKilled,
        StopReason::Cancelled,
        StopReason::Kill,
    ];
    for reason in &all {
        match reason {
            StopReason::Graceful
            | StopReason::ParentRequest
            | StopReason::Shutdown(_)
            | StopReason::Failure(_)
            | StopReason::ParentKilled
            | StopReason::Cancelled
            | StopReason::Kill => {}
        }
    }
    all
}

/// What the library is held to for one reason, stated apart from the
/// library's own classification.
struct Expect {
    /// A clean end: a `Transient` child is not restarted.
    normal: bool,
    /// `pre_stop` is asked, and may veto.
    vetoable: bool,
    display: &'static str,
}

fn expect(reason: &StopReason) -> Expect {
    match reason {
        StopReason::Graceful => Expect {
            normal: true,
            vetoable: true,
            display: "graceful",
        },
        StopReason::ParentRequest => Expect {
            normal: true,
            vetoable: true,
            display: "parent requested",
        },
        StopReason::Shutdown(_) => Expect {
            normal: true,
            vetoable: true,
            display: "shutdown: done",
        },
        StopReason::Failure(_) => Expect {
            normal: false,
            vetoable: false,
            display: "failure: actor logic error: f",
        },
        StopReason::ParentKilled => Expect {
            normal: true,
            vetoable: false,
            display: "parent killed",
        },
        StopReason::Cancelled => Expect {
            normal: false,
            vetoable: false,
            display: "cancelled",
        },
        StopReason::Kill => Expect {
            normal: false,
            vetoable: false,
            display: "killed",
        },
    }
}

// ---------------------------------------------------------------------------
// The rank table
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Msg {
    /// Park at the gate, then answer.
    Hold,
    Ping,
}

/// An actor that parks in a handler, so two requests land on its lane before
/// it reads either, and records what its hooks saw.
struct Parked {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    asked: Arc<Mutex<Vec<StopReason>>>,
    /// Vetoes every request `pre_stop` is asked about.
    veto: bool,
    asked_notify: Arc<Notify>,
    stopped_with: Arc<Mutex<Option<StopReason>>>,
}

struct ParkedRig {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    asked: Arc<Mutex<Vec<StopReason>>>,
    asked_notify: Arc<Notify>,
    stopped_with: Arc<Mutex<Option<StopReason>>>,
    handle: ActorHandle<Parked>,
}

impl Actor for Parked {
    type Message = Msg;
    type Response = u32;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        if let Msg::Hold = msg {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(1)
    }

    async fn pre_stop(&mut self, reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.asked.lock().unwrap().push(reason.clone());
        self.asked_notify.notify_one();
        !self.veto
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        *self.stopped_with.lock().unwrap() = Some(reason.clone());
        Ok(())
    }
}

async fn parked(veto: bool) -> ParkedRig {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let asked = Arc::new(Mutex::new(Vec::new()));
    let asked_notify = Arc::new(Notify::new());
    let stopped_with = Arc::new(Mutex::new(None));
    let handle = Parked {
        entered: entered.clone(),
        release: release.clone(),
        asked: asked.clone(),
        veto,
        asked_notify: asked_notify.clone(),
        stopped_with: stopped_with.clone(),
    }
    .spawn()
    .named(uname("parked"))
    .await
    .expect("the actor starts");
    ParkedRig {
        entered,
        release,
        asked,
        asked_notify,
        stopped_with,
        handle,
    }
}

impl ParkedRig {
    async fn ended(&self) -> StopReason {
        timeout(BOUND, self.handle.wait_stopped())
            .await
            .expect("the actor ends")
    }
}

// ---------------------------------------------------------------------------
// Every variant through every reader
// ---------------------------------------------------------------------------

/// The readers that need no actor: the text and the clean-end test.
#[test]
fn every_variant_through_display_and_is_normal() {
    for reason in every_reason() {
        let want = expect(&reason);
        assert_eq!(reason.to_string(), want.display, "{reason:?}");
        assert_eq!(reason.is_normal(), want.normal, "{reason:?}");
    }
}

/// The `pre_stop` gate: a vetoable request asks `pre_stop`, which here vetoes
/// and leaves the actor running; a forced one or a `Kill` never asks it and
/// ends the actor with the reason.
#[test]
fn every_variant_through_the_pre_stop_gate() {
    common::run(2, async {
        for reason in every_reason() {
            let want = expect(&reason);
            let rig = parked(true).await;
            rig.handle.stop(reason.clone()).await.expect("accepted");
            if want.vetoable {
                timeout(BOUND, rig.asked_notify.notified())
                    .await
                    .expect("pre_stop is asked");
                // The veto leaves the actor running: it still answers.
                let answer = timeout(BOUND, rig.handle.send(Msg::Ping))
                    .await
                    .expect("the actor answers");
                assert_eq!(answer.expect("a running actor"), 1, "{reason:?}");
                assert_eq!(*rig.asked.lock().unwrap(), vec![reason.clone()]);
                // A forced stop ends it all the same.
                rig.handle
                    .stop(StopReason::Cancelled)
                    .await
                    .expect("accepted");
                assert_eq!(rig.ended().await, StopReason::Cancelled, "{reason:?}");
            } else {
                assert_eq!(rig.ended().await, reason, "{reason:?}");
                assert!(
                    rig.asked.lock().unwrap().is_empty(),
                    "{reason:?}: pre_stop must not be asked"
                );
            }
        }
    });
}

/// A supervisor that starts one child and reports each child death.
struct Sup {
    restart: RestartType,
    child: Arc<Mutex<Option<ActorHandle<Kid>>>>,
    events: mpsc::UnboundedSender<ChildEvent>,
    starts: Arc<AtomicU64>,
}

#[derive(Clone)]
enum KidMsg {
    /// Stop through the context with the reason, then answer.
    StopSelf(StopReason),
}

struct Kid;

impl Actor for Kid {
    type Message = KidMsg;
    type Response = ();

    async fn handle(&mut self, msg: KidMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            KidMsg::StopSelf(reason) => {
                ctx.stop(reason).expect("the loop is running");
                Ok(())
            }
        }
    }
}

impl Actor for Sup {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let starts = self.starts.clone();
        let handle = ctx
            .spawn_child(move || {
                starts.fetch_add(1, Ordering::SeqCst);
                Kid
            })
            .restart_type(self.restart)
            .await?;
        *self.child.lock().unwrap() = Some(handle);
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let _ = self.events.send(event.clone());
        Ok(())
    }
}

struct SupRig {
    sup: ActorHandle<Sup>,
    child: ActorHandle<Kid>,
    events: mpsc::UnboundedReceiver<ChildEvent>,
    starts: Arc<AtomicU64>,
}

async fn supervised(restart: RestartType) -> SupRig {
    let (tx, events) = mpsc::unbounded_channel();
    let slot = Arc::new(Mutex::new(None));
    let starts = Arc::new(AtomicU64::new(0));
    let sup = Sup {
        restart,
        child: slot.clone(),
        events: tx,
        starts: starts.clone(),
    }
    .spawn()
    .named(uname("sup"))
    .supervisor()
    .await
    .expect("the supervisor starts");
    let child = slot.lock().unwrap().take().expect("the child started");
    SupRig {
        sup,
        child,
        events,
        starts,
    }
}

impl SupRig {
    /// Ends the supervisor, and with it the child.
    async fn finish(self) {
        let _ = self.sup.stop(StopReason::Cancelled).await;
        timeout(BOUND, self.sup.wait_stopped())
            .await
            .expect("the supervisor ends");
    }

    async fn next_event(&mut self) -> ChildEvent {
        timeout(BOUND, self.events.recv())
            .await
            .expect("the child's death is reported")
            .expect("the supervisor is alive")
    }
}

/// What a supervisor does with a dead child, stated apart from the library.
fn restarts(restart: RestartType, reason: &StopReason) -> bool {
    match restart {
        RestartType::Permanent => true,
        RestartType::Temporary => false,
        RestartType::Transient => !expect(reason).normal,
    }
}
