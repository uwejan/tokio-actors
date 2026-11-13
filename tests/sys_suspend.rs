//! `Sys::suspend` and `Sys::resume`: a suspended actor takes no message and,
//! when it supervises, handles no child death, but it still answers
//! `get_status`, stops on a stop request and ends on a `Kill`. A restart
//! starts a running incarnation.
//!
//! The negative observations (nothing was handled) are made after several
//! full passes of the actor's loop, each pass being a status answer, with a
//! short pause between them. A slow host can only make such a check miss a
//! fault, never fail a correct run.

mod common;

use std::future::IntoFuture;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, timeout, Instant};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorHandle, ActorResult, ActorSystem, ChildEvent, RestartType, Shutdown,
    StopReason, SupervisionAction, SupervisionConfig,
};

/// The longest a wait for a condition may take. It only limits how long a
/// real hang takes to report; a slow host must not read as one.
const BOUND: Duration = Duration::from_secs(60);

/// How long a stop may take when nothing in it waits for a window: far below
/// the shutdown window the children are given, so a ladder that escalated
/// would not fit.
const PROMPT: Duration = Duration::from_secs(10);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!(
        "sys-suspend-{base}-{}",
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

async fn within<F: IntoFuture>(what: &str, fut: F) -> F::Output {
    match timeout(BOUND, fut.into_future()).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {BOUND:?} waiting for: {what}"),
    }
}

async fn promptly<F: IntoFuture>(what: &str, fut: F) -> F::Output {
    match timeout(PROMPT, fut.into_future()).await {
        Ok(value) => value,
        Err(_) => panic!("not done within {PROMPT:?}: {what}"),
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

// ---------------------------------------------------------------------------
// The actors
// ---------------------------------------------------------------------------

/// What the actors saw, in the order they saw it.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Msg(u32),
    Tick,
    Ping,
    Child(SupervisionAction),
}

#[derive(Clone, Default)]
struct Probe {
    log: Arc<Mutex<Vec<Seen>>>,
    pre_stops: Arc<AtomicUsize>,
    child_log: Arc<Mutex<Vec<Seen>>>,
    child_pre_stops: Arc<AtomicUsize>,
    built: Arc<AtomicUsize>,
}

impl Probe {
    fn log(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
    }

    fn ticks(&self) -> usize {
        self.log()
            .iter()
            .filter(|seen| **seen == Seen::Tick)
            .count()
    }

    fn child_log(&self) -> Vec<Seen> {
        self.child_log.lock().unwrap().clone()
    }
}

#[derive(Clone)]
enum Msg {
    Push(u32),
    Tick,
}

/// The actor under test: records what it handles and what its supervisor
/// duty delivers, optionally supervises one permanent child and optionally
/// ticks on a recurring timer.
struct Subject {
    probe: Probe,
    child: Option<String>,
    ticker: Option<Duration>,
}

impl Actor for Subject {
    type Message = Msg;
    type Response = ();

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let seen = match msg {
            Msg::Push(n) => Seen::Msg(n),
            Msg::Tick => Seen::Tick,
        };
        self.probe.log.lock().unwrap().push(seen);
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some(name) = self.child.clone() {
            let probe = self.probe.clone();
            ctx.spawn_child(move || {
                probe.built.fetch_add(1, Ordering::SeqCst);
                Child {
                    probe: probe.clone(),
                }
            })
            .named(name)
            .restart_type(RestartType::Permanent)
            .shutdown(Shutdown::Timeout(Duration::from_secs(30)))
            .await?;
        }
        if let Some(interval) = self.ticker {
            ctx.schedule(Msg::Tick).every(interval).await?;
        }
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.probe
            .log
            .lock()
            .unwrap()
            .push(Seen::Child(event.action));
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.probe.pre_stops.fetch_add(1, Ordering::SeqCst);
        true
    }
}

#[derive(Clone)]
enum ChildMsg {
    Crash,
    Ping,
}

struct Child {
    probe: Probe,
}

impl Actor for Child {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Crash => panic!("child crashed on command"),
            ChildMsg::Ping => self.probe.child_log.lock().unwrap().push(Seen::Ping),
        }
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.probe.child_pre_stops.fetch_add(1, Ordering::SeqCst);
        true
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Plain,
    Supervisor,
}

struct Rig {
    sys: Arc<ActorSystem>,
    probe: Probe,
    subject: ActorHandle<Subject>,
    subject_name: String,
    child_name: String,
}

/// A subject with a priority lane on a system of its own. A supervisor kind
/// has one permanent child and a restart budget of one.
async fn rig(kind: Kind, ticker: Option<Duration>) -> Rig {
    rig_with_lane(kind, ticker, true).await
}

/// [`rig`], with or without the priority lane.
async fn rig_with_lane(kind: Kind, ticker: Option<Duration>, lane: bool) -> Rig {
    let sys = ActorSystem::create(uname("sys")).expect("system name must be unique");
    let probe = Probe::default();
    let subject_name = uname("subject");
    let child_name = uname("child");
    let mut builder = Subject {
        probe: probe.clone(),
        child: matches!(kind, Kind::Supervisor).then(|| child_name.clone()),
        ticker,
    }
    .spawn()
    .named(subject_name.clone())
    .on_system(&sys)
    // A `Kill` must be read by the loop: the abort that backs it up is far off.
    .kill_grace(Duration::from_secs(30))
    .with_config(if lane {
        ActorConfig::default().with_priority_lane(4)
    } else {
        ActorConfig::default()
    });
    if matches!(kind, Kind::Supervisor) {
        builder = builder.with_supervision(
            SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)),
        );
    }
    let subject = within("the subject to start", builder).await.unwrap();
    Rig {
        sys,
        probe,
        subject,
        subject_name,
        child_name,
    }
}

impl Rig {
    async fn child(&self) -> ActorHandle<Child> {
        let deadline = Instant::now() + BOUND;
        loop {
            if let Some(child) = self.sys.get::<Child>(&self.child_name) {
                if child.is_alive() {
                    return child;
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for the child");
            sleep(Duration::from_millis(2)).await;
        }
    }
}

/// Lets a suspended actor take several full passes of its loop: each
/// repeated `suspend` is served between two turns, and the pause lets any task
/// the actor spawned meanwhile run. A status answer is not used for this: it
/// turns the loop's fast path off for a while, which would hide a suspended
/// actor that takes the mailbox ahead of its select.
async fn quiet_suspended<A: Actor>(handle: &ActorHandle<A>) {
    for _ in 0..3 {
        within("a repeated suspend", handle.sys().suspend())
            .await
            .unwrap();
        sleep(Duration::from_millis(40)).await;
    }
}

/// Lets a running actor go on for a while.
async fn pause() {
    sleep(Duration::from_millis(150)).await;
}

fn record_of(sys: &ActorSystem, name: &str) -> tokio_actors::telemetry::ActorRecord {
    sys.snapshot()
        .into_iter()
        .find(|record| record.name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("{name} is not in the snapshot"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Rows: an actor with nothing on its priority lane, an actor with one, and
/// a supervisor with one. Messages sent to a suspended actor wait in the
/// lanes, the status shows them with `suspended`, the snapshot lists the
/// suspension, and one `resume` after two `suspend` calls lets them through,
/// the priority lane first and the mailbox in send order. The row without a
/// priority message is the one that takes the loop's fast path to the
/// mailbox.
#[test]
fn a_suspended_actor_takes_no_message_and_resumes_in_send_order() {
    for (kind, priority) in [
        (Kind::Plain, false),
        (Kind::Plain, true),
        (Kind::Supervisor, true),
    ] {
        common::run_or_leak(2, async move {
            let rig = rig(kind, None).await;
            let sys = rig.subject.sys();
            assert!(!record_of(&rig.sys, &rig.subject_name).suspended);

            sys.suspend().await.unwrap();
            for n in 1..=3 {
                rig.subject.notify(Msg::Push(n)).await.unwrap();
            }
            if priority {
                rig.subject.notify_priority(Msg::Push(100)).await.unwrap();
            }
            // Each repeated call is served with the messages already queued,
            // so the loop passes its top with a message waiting and no stop
            // or status work pending, and the fast path is not turned off.
            quiet_suspended(&rig.subject).await;
            let record = record_of(&rig.sys, &rig.subject_name);
            assert!(
                record.suspended,
                "{kind:?}: the snapshot lists the suspension"
            );
            assert_eq!(
                record.mailbox_len, 3,
                "{kind:?}: the mailbox holds its messages"
            );
            assert_eq!(record.priority_len, usize::from(priority));
            assert_eq!(rig.probe.log(), vec![], "{kind:?}: nothing was handled");
            // The status is answered while suspended.
            let info = rig.subject.get_status().await.unwrap();
            assert!(info.suspended, "{kind:?}: status reports the suspension");
            assert_eq!(info.mailbox_len, 3);
            assert_eq!(info.priority_len, usize::from(priority));

            // One resume ends two suspends.
            sys.resume().await.unwrap();
            let mut expected = vec![Seen::Msg(1), Seen::Msg(2), Seen::Msg(3)];
            if priority {
                expected.insert(0, Seen::Msg(100));
            }
            until("the held messages to be handled", || {
                rig.probe.log().len() == expected.len()
            })
            .await;
            assert_eq!(
                rig.probe.log(),
                expected,
                "{kind:?}: the lane first, then the mailbox in send order"
            );
            assert!(!rig.subject.get_status().await.unwrap().suspended);
            assert!(!record_of(&rig.sys, &rig.subject_name).suspended);
            // Resuming a running actor changes nothing.
            sys.resume().await.unwrap();
            assert!(!rig.subject.get_status().await.unwrap().suspended);
        });
    }
}

/// A recurring timer goes on delivering into the mailbox of a suspended
/// actor and nothing is handled, until the actor is resumed.
#[test]
fn a_timer_fills_the_mailbox_of_a_suspended_actor_and_nothing_is_handled() {
    common::run_or_leak(2, async {
        let rig = rig(Kind::Plain, Some(Duration::from_millis(5))).await;
        let sys = rig.subject.sys();
        sys.suspend().await.unwrap();
        let handled = rig.probe.ticks();
        until("ticks to queue in the mailbox", || {
            rig.subject.mailbox_len() >= 3
        })
        .await;
        quiet_suspended(&rig.subject).await;
        assert_eq!(
            rig.probe.ticks(),
            handled,
            "no tick is handled while suspended"
        );

        sys.resume().await.unwrap();
        until("a queued tick to be handled", || {
            rig.probe.ticks() > handled
        })
        .await;
    });
}

/// A suspended supervisor restarts nothing and delivers no `on_child_stopped`
/// until `resume`. Then the restart is decided once, charged once against a
/// budget of one, the death is delivered after that decision, and it is
/// delivered before a message that was queued behind it.
#[test]
fn a_suspended_supervisor_restarts_nothing_until_resumed_and_delivers_the_death_first() {
    common::run_or_leak(2, async {
        let rig = rig(Kind::Supervisor, None).await;
        let sys = rig.subject.sys();
        let child = rig.child().await;
        assert_eq!(rig.probe.built.load(Ordering::SeqCst), 1);

        sys.suspend().await.unwrap();
        child.notify(ChildMsg::Crash).await.unwrap();
        within("the child to end", child.wait_stopped()).await;
        rig.subject.notify(Msg::Push(1)).await.unwrap();

        quiet_suspended(&rig.subject).await;
        assert_eq!(
            rig.probe.built.load(Ordering::SeqCst),
            1,
            "no restart while suspended"
        );
        assert_eq!(
            rig.probe.log(),
            vec![],
            "no death and no message while suspended"
        );
        assert!(rig.subject.is_alive());

        sys.resume().await.unwrap();
        until("the death and the message", || rig.probe.log().len() == 2).await;
        assert_eq!(
            rig.probe.log(),
            vec![
                Seen::Child(SupervisionAction::RestartInitiated),
                Seen::Msg(1)
            ],
            "the death plane is taken before the mailbox"
        );
        until("the restarted incarnation", || {
            rig.probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let _ = rig.child().await;
        pause().await;
        assert_eq!(rig.probe.built.load(Ordering::SeqCst), 2, "restarted once");
        assert_eq!(rig.probe.log().len(), 2, "one death, one delivery");
        assert!(
            rig.subject.is_alive(),
            "one restart is within a budget of one"
        );
    });
}

/// A suspended actor still serves its stop lane. Rows: a graceful stop of an
/// actor and of a supervisor, a `Kill` of each, and a graceful stop of a
/// supervisor whose child is the suspended one. The graceful stop runs
/// `pre_stop` and ends with `Graceful` without a resume, a `Kill` ends with
/// `Kill` and runs no hook, and a ladder over a suspended child does not wait
/// for its window.
#[test]
fn a_suspended_actor_still_stops_on_a_stop_request_and_ends_on_kill() {
    struct Row {
        kind: Kind,
        suspend_child: bool,
        reason: StopReason,
    }
    let rows = [
        Row {
            kind: Kind::Plain,
            suspend_child: false,
            reason: StopReason::Graceful,
        },
        Row {
            kind: Kind::Supervisor,
            suspend_child: false,
            reason: StopReason::Graceful,
        },
        Row {
            kind: Kind::Supervisor,
            suspend_child: true,
            reason: StopReason::Graceful,
        },
        Row {
            kind: Kind::Plain,
            suspend_child: false,
            reason: StopReason::Kill,
        },
        Row {
            kind: Kind::Supervisor,
            suspend_child: false,
            reason: StopReason::Kill,
        },
    ];
    for row in rows {
        common::run_or_leak(2, async move {
            let what = format!(
                "{:?}, child suspended {}, {:?}",
                row.kind, row.suspend_child, row.reason
            );
            let rig = rig(row.kind, None).await;
            if row.suspend_child {
                let child = rig.child().await;
                child.sys().suspend().await.unwrap();
                assert!(child.get_status().await.unwrap().suspended);
            } else {
                rig.subject.sys().suspend().await.unwrap();
            }

            promptly("the stop request", rig.subject.stop(row.reason.clone()))
                .await
                .unwrap();
            let reason = promptly("the actor to end", rig.subject.wait_stopped()).await;
            assert_eq!(reason, row.reason, "{what}");
            let ran = rig.probe.pre_stops.load(Ordering::SeqCst);
            match row.reason {
                StopReason::Kill => assert_eq!(ran, 0, "{what}: a Kill runs no pre_stop"),
                _ => assert_eq!(ran, 1, "{what}: a stop request runs pre_stop"),
            }
            if row.suspend_child {
                assert_eq!(
                    rig.probe.child_pre_stops.load(Ordering::SeqCst),
                    1,
                    "{what}: the suspended child took the stop request"
                );
            }
        });
    }
}

/// A suspension is the incarnation's: a child that is suspended and then
/// killed comes back running and takes its messages.
#[test]
fn a_restarted_child_is_not_suspended() {
    common::run_or_leak(2, async {
        let rig = rig(Kind::Supervisor, None).await;
        let child = rig.child().await;
        child.sys().suspend().await.unwrap();
        assert!(child.get_status().await.unwrap().suspended);

        child.stop(StopReason::Kill).await.unwrap();
        within("the child to end", child.wait_stopped()).await;
        until("the restarted incarnation", || {
            rig.probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let next = rig.child().await;
        assert!(!next.get_status().await.unwrap().suspended);
        next.notify(ChildMsg::Ping).await.unwrap();
        until("the ping to be handled", || {
            rig.probe.child_log() == vec![Seen::Ping]
        })
        .await;
    });
}

/// Dropping every handle never ends an actor, so a suspended actor whose
/// handles are all gone stays suspended and findable by id, whether or not it
/// has a priority lane. Rows: a `kill_by_id` and a system shutdown end it,
/// with `Kill` and with `ParentRequest`, and the shutdown runs `pre_stop`
/// where the kill does not. Each end must come promptly, far inside the
/// actor's kill grace, so a stop the loop does not read fails with what the
/// actor shows instead of waiting for the abort.
#[test]
fn an_orphaned_suspended_actor_stays_suspended_until_a_kill_or_a_shutdown_ends_it() {
    struct Row {
        shutdown: bool,
        lane: bool,
        reason: StopReason,
        pre_stops: usize,
    }
    let mut rows = Vec::new();
    for lane in [false, true] {
        rows.push(Row {
            shutdown: false,
            lane,
            reason: StopReason::Kill,
            pre_stops: 0,
        });
        rows.push(Row {
            shutdown: true,
            lane,
            reason: StopReason::ParentRequest,
            pre_stops: 1,
        });
    }
    for row in rows {
        common::run_or_leak(2, async move {
            let what = format!("shutdown {}, lane {}", row.shutdown, row.lane);
            let rig = rig_with_lane(Kind::Plain, None, row.lane).await;
            let Rig {
                sys,
                probe,
                subject,
                ..
            } = rig;
            let id = subject.id().clone();
            subject.sys().suspend().await.unwrap();
            drop(subject);

            // Several passes of the empty, closed loop.
            for _ in 0..3 {
                pause().await;
            }
            let held = sys
                .get_by_id::<Subject>(&id)
                .unwrap_or_else(|| panic!("{what}: the orphan is not found by id"));
            let status = timeout(PROMPT, held.get_status())
                .await
                .unwrap_or_else(|_| {
                    panic!("{what}: the orphan did not answer a status request in {PROMPT:?}")
                })
                .unwrap();
            assert!(status.suspended, "{what}: the orphan is still suspended");
            assert_eq!(probe.pre_stops.load(Ordering::SeqCst), 0, "{what}");
            drop(held);

            let seen = {
                let (sys, id) = (sys.clone(), id.clone());
                move || match sys.get_by_id::<Subject>(&id) {
                    Some(held) => format!("still found by id, alive {}", held.is_alive()),
                    None => "not found by id".to_string(),
                }
            };
            let report = if row.shutdown {
                let Ok(report) = timeout(PROMPT, sys.shutdown()).await else {
                    panic!(
                        "{what}: the shutdown did not end the actor in {PROMPT:?}: {}",
                        seen()
                    );
                };
                assert_eq!(report.outcomes.len(), 1, "{what}");
                assert_eq!(report.outcomes[0].0, id, "{what}");
                report.outcomes[0].1.clone()
            } else {
                let Ok(ended) = timeout(PROMPT, sys.kill_by_id(&id)).await else {
                    panic!(
                        "{what}: the kill did not end the actor in {PROMPT:?}: {}",
                        seen()
                    );
                };
                ended.unwrap_or_else(|| panic!("{what}: the kill found no target"))
            };
            assert_eq!(report.reason, Some(row.reason), "{what}");
            assert_eq!(
                probe.pre_stops.load(Ordering::SeqCst),
                row.pre_stops,
                "{what}"
            );
            assert!(sys.get_by_id::<Subject>(&id).is_none(), "{what}: gone");
        });
    }
}
