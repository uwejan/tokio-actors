//! The system monitor through the public API: which configurations it
//! accepts, how a second monitor replaces the first, what clears it, and that
//! it leaves the turn warning threshold alone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, timeout, Instant};

use tokio_actors::telemetry::{
    Delivery, EventKind, HelperKind, KindSet, LongQueue, MonitorConfig, MonitorError,
    TelemetryEvent,
};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorSystem, StopReason,
};

mod common;

#[cfg(feature = "tracing")]
#[path = "common/capture.rs"]
mod capture;

const BOUND: Duration = Duration::from_secs(20);

fn uname(base: &str) -> String {
    format!("system-monitor-{base}-{}", uuid::Uuid::new_v4())
}

/// Forwards what the system tells it to the test.
struct Mon {
    tx: mpsc::UnboundedSender<Arc<TelemetryEvent<'static>>>,
}

impl Actor for Mon {
    type Message = Delivery;
    type Response = ();

    async fn handle(&mut self, msg: Delivery, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let _ = self.tx.send(msg.event);
        Ok(())
    }
}

/// Holds its turn open until the test adds a permit.
struct Held {
    entered: Arc<AtomicBool>,
    gate: Arc<Semaphore>,
}

impl Actor for Held {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.entered.store(true, Ordering::SeqCst);
        self.gate.acquire().await.unwrap().forget();
        Ok(())
    }
}

async fn spawn_mon(
    sys: &Arc<ActorSystem>,
) -> (
    ActorHandle<Mon>,
    mpsc::UnboundedReceiver<Arc<TelemetryEvent<'static>>>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mon = Mon { tx }.spawn().on_system(sys).await.unwrap();
    (mon, rx)
}

async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + BOUND;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(5)).await;
    }
}

fn schedule(ms: u64) -> MonitorConfig {
    MonitorConfig {
        long_schedule: Some(Duration::from_millis(ms)),
        long_message_queue: None,
    }
}

fn queue(enable: usize, disable: usize) -> MonitorConfig {
    MonitorConfig {
        long_schedule: None,
        long_message_queue: Some(LongQueue { disable, enable }),
    }
}

/// A configuration that cannot work, a monitor that has ended and a runtime
/// without the time driver are each refused with their own error, and the
/// monitor the system had stays.
#[test]
fn a_refused_call_names_its_reason_and_leaves_the_previous_monitor() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("validate")).unwrap();
        let (mon, _rx) = spawn_mon(&sys).await;
        assert_eq!(sys.system_monitor(&mon, |d| d, schedule(500)), Ok(None));
        let kept = sys.system_monitor_settings().expect("the monitor is set");

        let (ended, _ended_rx) = spawn_mon(&sys).await;
        ended.stop(StopReason::Graceful).await.unwrap();
        timeout(BOUND, ended.wait_stopped()).await.unwrap();

        // (monitor, configuration, error)
        let rows = [
            (&mon, queue(0, 0), MonitorError::ZeroEnable),
            (&mon, queue(5, 5), MonitorError::DisableNotBelowEnable),
            (&mon, queue(5, 9), MonitorError::DisableNotBelowEnable),
            (&ended, schedule(500), MonitorError::MonitorEnded),
        ];
        for (monitor, config, error) in rows {
            let got = sys.system_monitor(monitor, |d| d, config.clone());
            assert_eq!(got, Err(error), "{config:?}");
            assert_eq!(sys.system_monitor_settings().as_ref(), Some(&kept));
        }

        // A runtime built without the time driver cannot sleep for the
        // sampling.
        let (sys2, mon2) = (sys.clone(), mon.clone());
        let got = std::thread::spawn(move || {
            let bare = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            bare.block_on(async { sys2.system_monitor(&mon2, |d| d, schedule(500)) })
        })
        .join()
        .unwrap();
        assert_eq!(got, Err(MonitorError::NoTimeDriver));
        assert_eq!(sys.system_monitor_settings().as_ref(), Some(&kept));
    });
}

/// A second monitor returns the first's settings and replaces it, an empty
/// configuration and `clear_system_monitor` return what they remove, and a
/// monitor whose actor ends is cleared.
#[test]
fn a_monitor_is_replaced_cleared_and_dropped_with_its_actor() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("slot")).unwrap();
        let (a, _a_rx) = spawn_mon(&sys).await;
        let (b, _b_rx) = spawn_mon(&sys).await;
        assert_eq!(sys.system_monitor_settings(), None);
        let (ends_tx, mut ends) = mpsc::unbounded_channel();
        assert!(sys.attach_telemetry(
            "ends",
            KindSet::of(&[EventKind::HelperEnded]),
            move |event| {
                if let TelemetryEvent::HelperEnded {
                    kind: HelperKind::Monitor,
                    owner: Some(owner),
                    ..
                } = event
                {
                    let _ = ends_tx.send(owner.clone());
                }
            }
        ));

        assert_eq!(sys.system_monitor(&a, |d| d, schedule(300)), Ok(None));
        let first = sys.system_monitor_settings().unwrap();
        assert_eq!(first.monitor.id(), a.id());
        assert_eq!(first.config, schedule(300));

        let replaced = sys.system_monitor(&b, |d| d, queue(4, 1)).unwrap();
        assert_eq!(replaced, Some(first));
        let second = sys.system_monitor_settings().unwrap();
        assert_eq!(second.monitor.id(), b.id());
        // The replaced monitor's helper ends and leaves the new monitor in
        // place.
        let owner = timeout(BOUND, ends.recv()).await.unwrap().unwrap();
        assert_eq!(&owner, a.id());
        assert_eq!(sys.system_monitor_settings().as_ref(), Some(&second));
        assert_eq!(sys.clear_system_monitor(), Some(second));
        assert_eq!(sys.system_monitor_settings(), None);
        assert_eq!(sys.clear_system_monitor(), None);

        // An empty configuration clears.
        assert_eq!(sys.system_monitor(&a, |d| d, schedule(300)), Ok(None));
        let third = sys.system_monitor_settings().unwrap();
        let cleared = sys.system_monitor(&a, |d| d, MonitorConfig::default());
        assert_eq!(cleared, Ok(Some(third)));
        assert_eq!(sys.system_monitor_settings(), None);

        // The actor ends: the system forgets it.
        assert_eq!(sys.system_monitor(&b, |d| d, schedule(300)), Ok(None));
        b.stop(StopReason::Graceful).await.unwrap();
        timeout(BOUND, b.wait_stopped()).await.unwrap();
        until("the ended monitor to be cleared", || {
            sys.system_monitor_settings().is_none()
        })
        .await;
    });
}

/// A `long_schedule` makes the system time turns and report them to the
/// monitor, and touches neither the user's threshold nor what the user reads
/// through it.
#[test]
fn a_long_schedule_leaves_the_turn_warning_threshold_alone() {
    common::run(2, async {
        let sys = ActorSystem::create(uname("threshold")).unwrap();
        let (mon, mut rx) = spawn_mon(&sys).await;
        assert_eq!(sys.system_monitor(&mon, |d| d, schedule(50)), Ok(None));

        let entered = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Semaphore::new(0));
        let held = Held {
            entered: entered.clone(),
            gate: gate.clone(),
        }
        .spawn()
        .on_system(&sys)
        .await
        .unwrap();
        held.notify(()).await.unwrap();
        until("the turn to begin", || entered.load(Ordering::SeqCst)).await;

        // The monitor is told, so turns are timed.
        let told = timeout(BOUND, rx.recv()).await.unwrap().unwrap();
        assert_eq!(told.kind(), Some(EventKind::TurnStuck));
        let records = sys.snapshot();
        let record = records.iter().find(|r| r.actor.id() == held.id());
        assert!(
            record.is_some_and(|r| r.in_turn.is_some()),
            "the monitor's long_schedule makes a snapshot fill in `in_turn`"
        );

        // The user's own view is as the user left it.
        assert_eq!(sys.turn_warning_threshold(), None);
        assert!(sys.stuck_actors().is_empty());
        assert_eq!(sys.turn_elapsed(held.id()), None);

        // And a threshold the user sets is the user's, whatever the monitor
        // holds.
        sys.set_turn_warning_threshold(Some(Duration::from_secs(3600)));
        assert_eq!(
            sys.turn_warning_threshold(),
            Some(Duration::from_secs(3600))
        );
        assert!(sys.stuck_actors().is_empty());

        gate.add_permits(1);
    });
}

/// Panics with its text when dropped, unless the thread is already unwinding.
#[cfg(feature = "tracing")]
struct Bomb(&'static str);

#[cfg(feature = "tracing")]
impl Drop for Bomb {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            panic!("{}", self.0);
        }
    }
}

/// The monitor's task owns the caller's `map`, and the map's state is dropped
/// when the monitored actor ends. A panic of that drop is logged with the
/// actor's id, and its text only for an actor that is not sensitive: for a
/// sensitive actor the line carries the word `panic`.
#[cfg(feature = "tracing")]
#[test]
fn a_map_that_panics_when_dropped_is_logged_without_its_text_for_a_sensitive_actor() {
    const SECRET: &str = "monitor-map-drop-text";
    capture::install();
    for multi in [false, true] {
        for sensitive in [false, true] {
            let runtime = if multi {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
            } else {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
            }
            .expect("a runtime");
            let name = uname(&format!("map-drop-{multi}-{sensitive}"));
            let row = format!("multi {multi}, sensitive {sensitive}");
            runtime.block_on(async {
                let sys = ActorSystem::create(uname("map-drop")).unwrap();
                let (tx, _rx) = mpsc::unbounded_channel();
                let builder = Mon { tx }.spawn().named(name.clone());
                let builder = if sensitive {
                    builder.sensitive()
                } else {
                    builder
                };
                let mon = builder.on_system(&sys).await.unwrap();
                let bomb = Bomb(SECRET);
                sys.system_monitor(
                    &mon,
                    move |delivery| {
                        let _ = &bomb;
                        delivery
                    },
                    schedule(500),
                )
                .unwrap();
                mon.stop(StopReason::Graceful).await.unwrap();
                timeout(BOUND, mon.wait_stopped()).await.unwrap();
                capture::until(&format!("{row}: the monitor helper's end line"), || {
                    !capture::events(|seen| {
                        seen.field("actor_id") == Some(name.as_str())
                            && seen.field("helper").is_some()
                    })
                    .is_empty()
                })
                .await;
            });
            let lines = capture::events(|seen| seen.field("actor_id") == Some(name.as_str()));
            let leaked = lines.iter().any(|seen| seen.line().contains(SECRET));
            assert_eq!(
                leaked,
                !sensitive,
                "{row}: the panic text is in the log exactly for an ordinary actor: {:?}",
                lines.iter().map(capture::Seen::line).collect::<Vec<_>>()
            );
            let helper = lines
                .iter()
                .find(|seen| seen.field("helper").is_some())
                .expect("the helper's line");
            assert_eq!(
                helper.field("panic"),
                Some(if sensitive { "panic" } else { SECRET }),
                "{row}: the helper's line"
            );
        }
    }
}
