//! Tests of the handler list: attach and detach by id, a handler that panics,
//! and a handler that calls back into the system.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::event::{EventKind, HelperEnd, HelperKind, KindSet, TelemetryEvent};
use super::super::mode::Mode;
use crate::system::ActorSystem;

/// How long a test waits for a body that may deadlock.
const BOUND: Duration = Duration::from_secs(20);

fn system() -> Arc<ActorSystem> {
    ActorSystem::create(format!("handlers-{}", uuid::Uuid::new_v4())).unwrap()
}

fn mode(system: &ActorSystem) -> Arc<Mode> {
    system.telemetry_mode().clone()
}

/// The event every test emits; its kind is `HelperEnded`.
fn helper_ended() -> TelemetryEvent<'static> {
    TelemetryEvent::HelperEnded {
        kind: HelperKind::Reaper,
        owner: None,
        end: HelperEnd::Finished,
    }
}

fn emit_bounded(mode: &Arc<Mode>) {
    let mode = mode.clone();
    bounded("emit", move || emit(&mode));
}

fn emit(mode: &Mode) {
    mode.dispatch(EventKind::HelperEnded, helper_ended);
}

fn ended() -> KindSet {
    KindSet::of(&[EventKind::HelperEnded])
}

fn ended_and_detached() -> KindSet {
    KindSet::of(&[EventKind::HelperEnded, EventKind::HandlerDetached])
}

/// What a recording handler saw.
type Seen = Arc<Mutex<Vec<String>>>;

fn label(event: &TelemetryEvent<'_>) -> String {
    match event {
        TelemetryEvent::HandlerDetached { handler, panic } => {
            format!("detached:{handler}:{panic}")
        }
        TelemetryEvent::HelperEnded { .. } => "ended".to_string(),
        _ => "other".to_string(),
    }
}

fn record(seen: &Seen, name: &str) -> impl Fn(&TelemetryEvent<'_>) + Send + Sync + 'static {
    let seen = seen.clone();
    let name = name.to_string();
    move |event| {
        seen.lock()
            .unwrap()
            .push(format!("{name}:{}", label(event)));
    }
}

/// Runs `body` on a thread of its own and fails the test when it does not end
/// within `BOUND` (a deadlock in the code under test).
fn bounded(what: &str, body: impl FnOnce() + Send + 'static) {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        body();
        let _ = tx.send(());
    });
    match rx.recv_timeout(BOUND) {
        Ok(()) => worker.join().unwrap(),
        Err(_) => panic!("{what}: the body panicked or did not end within {BOUND:?}"),
    }
}

/// A value whose drop panics, counting the drops. The panic skips the panic
/// hook: a drop that panics while the thread already unwinds is a second panic
/// in flight, and the default hook then prints a symbolized backtrace of the
/// whole test binary under a lock every panicking thread of the process
/// shares, which holds up all the other tests that panic until it is done.
struct Bomb(Arc<AtomicUsize>);

impl Drop for Bomb {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::panic::resume_unwind(Box::new("bomb dropped"));
    }
}

#[test]
fn handlers_take_only_their_kinds_in_attach_order() {
    let sys = system();
    let mode = mode(&sys);
    let seen: Seen = Arc::default();
    assert!(sys.attach_telemetry("first", ended(), record(&seen, "first")));
    assert!(sys.attach_telemetry(
        "other",
        KindSet::of(&[EventKind::StopRequested]),
        record(&seen, "other")
    ));
    assert!(sys.attach_telemetry("second", ended(), record(&seen, "second")));
    emit_bounded(&mode);
    assert_eq!(*seen.lock().unwrap(), ["first:ended", "second:ended"]);
}

#[test]
fn a_handler_that_panics_is_reported_once_with_its_text() {
    let sys = system();
    let mode = mode(&sys);
    let reports = mode.collect(KindSet::of(&[EventKind::HandlerDetached]));
    // (payload thrown, text reported)
    let table: [(&str, fn(), &str); 3] = [
        ("str", || panic!("plain"), "plain"),
        (
            "string",
            || panic!("{}", String::from("formatted")),
            "formatted",
        ),
        (
            "any",
            || std::panic::panic_any(7u32),
            "non-string panic payload",
        ),
    ];
    for (id, throw, text) in table {
        assert!(sys.attach_telemetry(id, ended(), move |_| throw()));
        emit_bounded(&mode);
        let events = reports.events();
        match events.last() {
            Some(TelemetryEvent::HandlerDetached { handler, panic }) => {
                assert_eq!(handler, id);
                assert_eq!(panic, text);
            }
            other => panic!("no report for {id}: {other:?}"),
        }
    }
    assert_eq!(reports.events().len(), 3, "one report per detached handler");
}

#[test]
fn a_handler_can_attach_detach_and_emit_inside_itself() {
    let sys = system();
    let mode = mode(&sys);
    let seen: Seen = Arc::default();
    let inner = seen.clone();
    let (s, m) = (sys.clone(), mode.clone());
    let outer_calls = Arc::new(AtomicUsize::new(0));
    let oc = outer_calls.clone();
    assert!(sys.attach_telemetry("outer", ended(), move |_| {
        oc.fetch_add(1, Ordering::SeqCst);
        // attach: effective from the next event, not this one.
        assert!(s.attach_telemetry("late", ended(), record(&inner, "late")));
        assert!(!s.attach_telemetry("late", ended(), |_| {}));
        // detach of another handler already in this event's snapshot: that
        // handler still runs for this event, never for the next.
        assert!(s.detach_telemetry("victim"));
        // emit: reaches no handler, does not recurse, does not deadlock.
        emit(&m);
        // detach of itself.
        assert!(s.detach_telemetry("outer"));
    }));
    assert!(sys.attach_telemetry("victim", ended(), record(&seen, "victim")));
    let m = mode.clone();
    bounded("re-entrant calls", move || emit(&m));
    assert_eq!(
        outer_calls.load(Ordering::SeqCst),
        1,
        "the inner emit reached no handler"
    );
    assert_eq!(*seen.lock().unwrap(), ["victim:ended"]);
    seen.lock().unwrap().clear();
    let m = mode.clone();
    bounded("second emit", move || emit(&m));
    assert_eq!(
        outer_calls.load(Ordering::SeqCst),
        1,
        "outer detached itself"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        ["late:ended"],
        "victim is gone, late is live"
    );
}

#[test]
fn re_attaching_from_the_report_loops_and_the_test_bounds_it() {
    let sys = system();
    let mode = mode(&sys);
    let rounds = Arc::new(AtomicUsize::new(0));
    let s = sys.clone();
    let r = rounds.clone();
    // Each handler, when it is reported detached, attaches the next one under
    // a new id and that one panics at once; the counter ends the chain.
    fn arm(sys: &Arc<ActorSystem>, rounds: &Arc<AtomicUsize>, n: usize) {
        let (s, r) = (sys.clone(), rounds.clone());
        sys.attach_telemetry(
            &format!("h{n}"),
            KindSet::of(&[EventKind::HelperEnded, EventKind::HandlerDetached]),
            move |event| {
                if matches!(event, TelemetryEvent::HandlerDetached { .. }) {
                    return;
                }
                r.fetch_add(1, Ordering::SeqCst);
                if r.load(Ordering::SeqCst) < 5 {
                    arm(&s, &r, n + 1);
                }
                panic!("loop");
            },
        );
    }
    arm(&s, &r, 0);
    let m = mode.clone();
    bounded("re-attach loop", move || {
        // One emission per step; each step detaches one handler.
        for _ in 0..5 {
            emit(&m);
        }
    });
    assert_eq!(rounds.load(Ordering::SeqCst), 5);
}

/// Counts its drop.
struct Guard(Arc<AtomicUsize>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
