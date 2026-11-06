//! A refused `spawn_child` must not widen the wait bound of the subtree it
//! never joined, whichever way it is refused: a name taken in the system, a
//! child of the same id under the supervisor, or a system that is shutting
//! down.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, RestartType, Shutdown, ShutdownPolicy, StopReason,
};

#[derive(Clone)]
enum GMsg {
    Block,
}

struct G;
impl Actor for G {
    type Message = GMsg;
    type Response = ();
    async fn handle(&mut self, _m: GMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        // A handler that does not yield: neither Kill nor abort can reach it.
        std::thread::sleep(Duration::from_secs(8));
        Ok(())
    }
}

struct Plain;
impl Actor for Plain {
    type Message = ();
    type Response = ();
    async fn handle(&mut self, _m: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A root that vetoes every stop request and reports each one on `asked`.
struct Gate {
    asked: mpsc::UnboundedSender<()>,
}
impl Actor for Gate {
    type Message = ();
    type Response = ();
    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        let _ = self.asked.send(());
        false
    }
    async fn handle(&mut self, _m: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[derive(Clone)]
enum CMsg {
    SpawnG,
    SpawnTwin,
    SpawnRefused(&'static str),
}

struct C;
impl Actor for C {
    type Message = CMsg;
    type Response = String;
    async fn handle(&mut self, m: CMsg, ctx: &mut ActorContext<Self>) -> ActorResult<String> {
        Ok(match m {
            CMsg::SpawnG => ctx
                .spawn_child(|| G)
                .named("g")
                .restart_type(RestartType::Temporary)
                .shutdown(Shutdown::Timeout(Duration::from_millis(60)))
                .kill_grace(Duration::from_millis(30))
                .await
                .map(|_| "ok".to_string())
                .unwrap_or_else(|e| e.to_string()),
            CMsg::SpawnTwin => ctx
                .spawn_child(|| Plain)
                .named("twin")
                .await
                .map(|_| "ok".to_string())
                .unwrap_or_else(|e| e.to_string()),
            CMsg::SpawnRefused(name) => ctx
                .spawn_child(|| Plain)
                .named(name)
                .shutdown(Shutdown::Timeout(Duration::from_secs(3600)))
                .kill_grace(Duration::from_secs(3600))
                .await
                .map(|_| "ok".to_string())
                .unwrap_or_else(|e| e.to_string()),
        })
    }
}

#[derive(Clone)]
enum RMsg {
    SpawnC,
    TerminateC,
}

struct R;
impl Actor for R {
    type Message = RMsg;
    type Response = String;
    async fn handle(&mut self, m: RMsg, ctx: &mut ActorContext<Self>) -> ActorResult<String> {
        Ok(match m {
            RMsg::SpawnC => ctx
                .spawn_child(|| C)
                .named("c")
                .shutdown(Shutdown::Timeout(Duration::from_secs(2)))
                .with_config(tokio_actors::ActorConfig::default().supervisor())
                .await
                .map(|_| "ok".to_string())
                .unwrap_or_else(|e| e.to_string()),
            RMsg::TerminateC => ctx
                .terminate_child("c")
                .await
                .map(|_| "ok".to_string())
                .unwrap_or_else(|e| e.to_string()),
        })
    }
}

/// How the refused spawn is refused.
#[derive(Clone, Copy)]
enum Refusal {
    /// The name belongs to an actor of the system outside the supervisor.
    NameTaken,
    /// A child of the supervisor already has the id.
    DuplicateChild,
    /// The system entered a shutdown that is still in flight.
    SystemShuttingDown,
}

impl Refusal {
    fn text(self) -> &'static str {
        match self {
            Refusal::NameTaken => "already taken",
            Refusal::DuplicateChild => "already registered",
            Refusal::SystemShuttingDown => "shutting down",
        }
    }
}

/// Starts a shutdown whose walk waits on a root that vetoes, then ends the
/// runtime the walk runs on: the system keeps refusing registrations while
/// every other root keeps running.
async fn enter_a_shutdown_that_stays_in_flight(sys: &std::sync::Arc<ActorSystem>) {
    let (asked_tx, mut asked_rx) = mpsc::unbounded_channel();
    // Registered after `r`, so the walk reaches it first and waits there.
    Gate { asked: asked_tx }
        .spawn()
        .named("gate")
        .on_system(sys)
        .await
        .unwrap();
    let walker = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let call = sys.clone();
    let _walk = walker.spawn(async move {
        call.shutdown_with(ShutdownPolicy {
            timeout: Duration::from_secs(600),
            per_actor_timeout: Duration::from_secs(600),
        })
        .await
    });
    timeout(Duration::from_secs(30), asked_rx.recv())
        .await
        .expect("the walk asked the gate within the bound")
        .expect("the gate reports its request");
    walker.shutdown_background();
}

async fn run(refusal: Option<Refusal>, label: &str) -> (String, Duration) {
    let sys = ActorSystem::create(format!("refused-budget-{label}")).unwrap();
    let _taken = Plain.spawn().named("taken").on_system(&sys).await.unwrap();
    let r = R
        .spawn()
        .named("r")
        .on_system(&sys)
        .supervisor()
        .await
        .unwrap();
    assert_eq!(r.send(RMsg::SpawnC).await.unwrap(), "ok");
    let c = loop {
        if let Some(c) = sys.get::<C>("c") {
            break c;
        }
        sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(c.send(CMsg::SpawnG).await.unwrap(), "ok");
    if matches!(refusal, Some(Refusal::DuplicateChild)) {
        assert_eq!(c.send(CMsg::SpawnTwin).await.unwrap(), "ok");
    }
    let g = loop {
        if let Some(g) = sys.get::<G>("g") {
            break g;
        }
        sleep(Duration::from_millis(5)).await;
    };
    g.notify(GMsg::Block).await.unwrap();
    // The loop calls `handle` in the poll that takes the message, so an empty
    // mailbox means G is inside its blocking handler.
    let deadline = Instant::now() + Duration::from_secs(10);
    while g.mailbox_len() != 0 {
        assert!(Instant::now() < deadline, "G never took the Block message");
        sleep(Duration::from_millis(1)).await;
    }
    if let Some(refusal) = refusal {
        if matches!(refusal, Refusal::SystemShuttingDown) {
            enter_a_shutdown_that_stays_in_flight(&sys).await;
        }
        let name = match refusal {
            Refusal::DuplicateChild => "twin",
            _ => "taken",
        };
        let out = c.send(CMsg::SpawnRefused(name)).await.unwrap();
        assert!(out.contains(refusal.text()), "the spawn is refused: {out}");
    }
    let t0 = Instant::now();
    let out = tokio::time::timeout(Duration::from_secs(20), r.send(RMsg::TerminateC)).await;
    let took = t0.elapsed();
    let out = match out {
        Ok(Ok(s)) => s,
        other => format!("no answer: {other:?}"),
    };
    (out, took)
}

fn assert_a_refused_spawn_does_not_widen_the_wait(refusal: Refusal, label: &str) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let (base, base_took) = rt.block_on(run(None, &format!("{label}-base")));
    eprintln!("baseline: {base:?} after {base_took:?}");
    let (refused, refused_took) = rt.block_on(run(Some(refusal), &format!("{label}-refused")));
    eprintln!("after a refused spawn_child(Timeout(1h)): {refused:?} after {refused_took:?}");
    rt.shutdown_background();
    assert!(
        base_took < Duration::from_secs(3),
        "baseline wait is bounded by the real subtree: {base_took:?}"
    );
    assert!(
        refused_took < Duration::from_secs(3),
        "the wait after a refused spawn is still bounded by the real subtree: {refused_took:?} ({refused})"
    );
}

#[test]
fn a_refused_spawn_does_not_widen_the_subtree_wait() {
    assert_a_refused_spawn_does_not_widen_the_wait(Refusal::NameTaken, "taken");
}

#[test]
fn a_duplicate_child_spawn_does_not_widen_the_subtree_wait() {
    assert_a_refused_spawn_does_not_widen_the_wait(Refusal::DuplicateChild, "duplicate");
}

#[test]
fn a_spawn_refused_by_a_shutdown_does_not_widen_the_subtree_wait() {
    assert_a_refused_spawn_does_not_widen_the_wait(Refusal::SystemShuttingDown, "shutting-down");
}
