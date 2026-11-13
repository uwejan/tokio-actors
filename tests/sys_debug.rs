//! The debug bundle of an actor: statistics, the log, the trace and the
//! installed functions, turned on by a spawn or by a call, on both runtime
//! flavors.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio_actors::actor::context::ActorContext;
use tokio_actors::sys::{DebugEntry, DebugEvent, DebugOptions, Install, DEFAULT_LOG_CAPACITY};
use tokio_actors::telemetry::{EventKind, KindSet, TelemetryEvent};
use tokio_actors::{
    Actor, ActorConfig, ActorError, ActorExt, ActorHandle, ActorResult, ActorSystem,
};

/// The longest a wait for a condition may take. It only limits how long a
/// real hang takes to report.
const HANG_BOUND: Duration = Duration::from_secs(30);

fn unique(base: &str) -> String {
    format!("sys-debug-{base}-{}", uuid::Uuid::new_v4())
}

/// Runs the scenario on a current-thread runtime and on a two-worker one.
fn both_flavors<F, Fut>(scenario: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(scenario());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(scenario());
}

/// Returns once the actor has finished the turn of every message sent so far:
/// a call is served at a turn boundary, after the reply and the entry that
/// follows it.
async fn settled(handle: &ActorHandle<Probe>) {
    handle.sys().statistics().await.unwrap();
}

async fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + HANG_BOUND;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A handler that parks until the test opens the gate, and opens it again on
/// every path out of the test.
struct Gate {
    entered: Semaphore,
    release: Semaphore,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }
}

struct OpenOnDrop(Arc<Gate>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.release.add_permits(1 << 20);
    }
}

#[derive(Clone, Copy)]
enum Msg {
    /// Answers 1.
    Add,
    /// Answers 1 once the gate is open.
    Hold,
    /// Records that the handler ran, and answers 1.
    Note,
    /// Fails.
    Fail,
}

struct Probe {
    gate: Arc<Gate>,
    notes: Arc<Mutex<Vec<&'static str>>>,
}

impl Probe {
    fn new() -> Self {
        Self {
            gate: Gate::new(),
            notes: Arc::default(),
        }
    }
}

impl Actor for Probe {
    type Message = Msg;
    type Response = u64;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
        match msg {
            Msg::Add => {}
            Msg::Hold => {
                self.gate.entered.add_permits(1);
                self.gate.release.acquire().await.unwrap().forget();
            }
            Msg::Note => self.notes.lock().unwrap().push("handle"),
            Msg::Fail => return Err(ActorError::user("told to fail")),
        }
        Ok(1)
    }
}

async fn spawn(
    system: &Arc<ActorSystem>,
    probe: Probe,
    options: DebugOptions,
) -> ActorHandle<Probe> {
    probe
        .spawn()
        .named(unique("probe"))
        .on_system(system)
        .debug(options)
        .await
        .unwrap()
}

fn system() -> Arc<ActorSystem> {
    ActorSystem::create(unique("system")).unwrap()
}

// ---------------------------------------------------------------------------
// Counts
// ---------------------------------------------------------------------------

async fn the_counts_follow_the_messages() {
    let system = system();
    // (sends, notifies); a run ends with a send, so every earlier notify has
    // been handled when the counts are read.
    let rows: [(u64, u64); 3] = [(1, 0), (3, 2), (2, 5)];
    for (sends, notifies) in rows {
        let handle = spawn(
            &system,
            Probe::new(),
            DebugOptions::default().statistics(true),
        )
        .await;
        for _ in 0..notifies {
            handle.notify(Msg::Add).await.unwrap();
        }
        for _ in 0..sends {
            handle.send(Msg::Add).await.unwrap();
        }
        let stats = handle.sys().statistics().await.unwrap().unwrap();
        assert_eq!(
            stats.messages_in,
            sends + notifies,
            "in for {sends}/{notifies}"
        );
        assert_eq!(stats.messages_out, sends, "out for {sends}/{notifies}");
        assert_eq!(
            stats.turns,
            sends + notifies,
            "turns for {sends}/{notifies}"
        );
    }

    // Off until asked: no statistics, then fresh ones from the call on.
    let handle = spawn(&system, Probe::new(), DebugOptions::default()).await;
    handle.send(Msg::Add).await.unwrap();
    assert_eq!(handle.sys().statistics().await.unwrap(), None);
    handle.sys().set_statistics(true).await.unwrap();
    handle.send(Msg::Add).await.unwrap();
    let stats = handle.sys().statistics().await.unwrap().unwrap();
    assert_eq!((stats.messages_in, stats.messages_out), (1, 1));
    handle.sys().set_statistics(false).await.unwrap();
    assert_eq!(handle.sys().statistics().await.unwrap(), None);
}

#[test]
fn the_counts_follow_the_messages_on_both_flavors() {
    both_flavors(the_counts_follow_the_messages);
}

// ---------------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------------

fn assert_in_order(entries: &[DebugEntry]) {
    for pair in entries.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq + 1, "oldest first, no gaps");
    }
}

async fn the_log_keeps_the_last_entries_oldest_first_within_the_clamp() {
    let system = system();
    // (capacity asked, sends, entries kept): a send makes two entries.
    let rows: [(usize, usize, usize); 4] = [(3, 5, 3), (0, 2, 1), (5000, 600, 1024), (10, 12, 10)];
    for (asked, sends, kept) in rows {
        let handle = spawn(&system, Probe::new(), DebugOptions::default()).await;
        assert_eq!(handle.sys().log().await.unwrap(), None, "off until asked");
        handle.sys().set_log(Some(asked)).await.unwrap();
        for _ in 0..sends {
            handle.send(Msg::Add).await.unwrap();
        }
        let entries = handle.sys().log().await.unwrap().unwrap();
        assert_eq!(entries.len(), kept, "kept for {asked}");
        assert_in_order(&entries);
        assert_eq!(
            entries.last().unwrap().event,
            DebugEvent::MessageOut { delivered: true }
        );
        assert_eq!(entries.last().unwrap().seq, 2 * sends as u64);
    }

    // The default capacity, and a preset log that is clamped as a call is.
    assert_eq!(DEFAULT_LOG_CAPACITY, 10);
    let handle = spawn(&system, Probe::new(), DebugOptions::default().log(0)).await;
    for _ in 0..3 {
        handle.send(Msg::Add).await.unwrap();
    }
    assert_eq!(handle.sys().log().await.unwrap().unwrap().len(), 1);
    handle.sys().set_log(None).await.unwrap();
    assert_eq!(handle.sys().log().await.unwrap(), None);
}

#[test]
fn the_log_keeps_the_last_entries_on_both_flavors() {
    both_flavors(the_log_keeps_the_last_entries_oldest_first_within_the_clamp);
}

async fn an_entry_names_the_reply_and_the_lane() {
    let system = system();
    let handle = Probe::new()
        .spawn()
        .named(unique("lanes"))
        .on_system(&system)
        .with_config(ActorConfig::default().with_priority_lane(4))
        .debug(DebugOptions::default().log(10))
        .await
        .unwrap();
    handle.send(Msg::Add).await.unwrap();
    handle.notify(Msg::Add).await.unwrap();
    handle.send(Msg::Add).await.unwrap();
    handle.send_priority(Msg::Add).await.unwrap();
    let events: Vec<DebugEvent> = handle
        .sys()
        .log()
        .await
        .unwrap()
        .unwrap()
        .into_iter()
        .map(|entry| entry.event)
        .collect();
    let message_in = |reply_expected, priority| DebugEvent::MessageIn {
        reply_expected,
        priority,
    };
    let out = DebugEvent::MessageOut { delivered: true };
    assert_eq!(
        events,
        vec![
            message_in(true, false),
            out.clone(),
            message_in(false, false),
            message_in(true, false),
            out.clone(),
            message_in(true, true),
            out,
        ]
    );
}

#[test]
fn an_entry_names_the_reply_and_the_lane_on_both_flavors() {
    both_flavors(an_entry_names_the_reply_and_the_lane);
}

// ---------------------------------------------------------------------------
// A toggle waits for the turn boundary; a preset starts each incarnation
// ---------------------------------------------------------------------------

async fn a_toggle_sent_during_a_handler_takes_effect_for_the_next_message() {
    let system = system();
    let probe = Probe::new();
    let gate = probe.gate.clone();
    let _open = OpenOnDrop(gate.clone());
    let handle = spawn(&system, probe, DebugOptions::default()).await;

    let held = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.send(Msg::Hold).await })
    };
    gate.entered.acquire().await.unwrap().forget();

    // One poll queues the call behind the running handler.
    let mut toggle = Box::pin({
        let handle = handle.clone();
        async move { handle.sys().set_statistics(true).await }
    });
    std::future::poll_fn(|cx| {
        let _ = toggle.as_mut().poll(cx);
        std::task::Poll::Ready(())
    })
    .await;

    gate.release.add_permits(1);
    held.await.unwrap().unwrap();
    toggle.await.unwrap();
    handle.send(Msg::Add).await.unwrap();
    let stats = handle.sys().statistics().await.unwrap().unwrap();
    assert_eq!(
        (stats.messages_in, stats.messages_out, stats.turns),
        (1, 1, 1),
        "only the message after the call is counted"
    );
}

#[test]
fn a_toggle_sent_during_a_handler_waits_on_both_flavors() {
    both_flavors(a_toggle_sent_during_a_handler_takes_effect_for_the_next_message);
}

/// A supervised child that fails when told to.
struct Child;

impl Actor for Child {
    type Message = Msg;
    type Response = u64;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
        match msg {
            Msg::Fail => Err(ActorError::user("told to fail")),
            _ => Ok(1),
        }
    }
}

/// Spawns a [`Child`] with the statistics on.
struct Parent;

impl Actor for Parent {
    type Message = String;
    type Response = ActorHandle<Child>;

    async fn handle(
        &mut self,
        name: String,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<ActorHandle<Child>> {
        Ok(ctx
            .spawn_child(|| Child)
            .named(name)
            .debug(DebugOptions::default().statistics(true))
            .await?)
    }
}

async fn a_restart_starts_from_the_preset_and_keeps_no_toggle() {
    let system = system();
    let parent = Parent
        .spawn()
        .named(unique("parent"))
        .on_system(&system)
        .supervisor()
        .await
        .unwrap();
    let name = unique("child");
    let child = parent.send(name.clone()).await.unwrap();

    // The preset is on at spawn; a toggle that is not in it is added.
    child.send(Msg::Add).await.unwrap();
    child.sys().set_log(Some(5)).await.unwrap();
    child.send(Msg::Add).await.unwrap();
    let stats = child.sys().statistics().await.unwrap().unwrap();
    assert_eq!(stats.messages_in, 2, "the preset counted from the spawn");
    assert_eq!(child.sys().log().await.unwrap().unwrap().len(), 2);

    child.notify(Msg::Fail).await.unwrap();
    // The next incarnation has fresh statistics from the preset.
    let deadline = Instant::now() + HANG_BOUND;
    let next = loop {
        assert!(Instant::now() < deadline, "the child was restarted");
        if let Some(next) = system.get::<Child>(&name) {
            if let Ok(Some(stats)) = next.sys().statistics().await {
                if stats.messages_in == 0 {
                    break next;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(
        next.sys().log().await.unwrap(),
        None,
        "the toggle did not survive"
    );
}

#[test]
fn a_restart_starts_from_the_preset_and_keeps_no_toggle_on_both_flavors() {
    both_flavors(a_restart_starts_from_the_preset_and_keeps_no_toggle);
}

// ---------------------------------------------------------------------------
// Installed functions
// ---------------------------------------------------------------------------

/// A value whose drop panics.
struct PanicOnDrop;

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        panic!("dropped");
    }
}

async fn installed_functions_run_in_order_and_one_that_fails_leaves_the_others() {
    let system = system();
    let probe = Probe::new();
    let notes = probe.notes.clone();
    let handle = spawn(&system, probe, DebugOptions::default()).await;

    // A function that panics is dropped alone; its neighbours keep running.
    let seen = Arc::new(AtomicUsize::new(0));
    handle
        .sys()
        .install("boom", |_| panic!("an installed function panics"))
        .await
        .unwrap();
    let counter = seen.clone();
    handle
        .sys()
        .install("count", move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Install::Keep
        })
        .await
        .unwrap();
    handle.send(Msg::Add).await.unwrap();
    assert!(
        !handle.sys().remove("boom").await.unwrap(),
        "dropped by its panic"
    );
    assert_eq!(
        seen.load(Ordering::SeqCst),
        2,
        "the entry reached the next function"
    );
    handle.send(Msg::Add).await.unwrap();
    settled(&handle).await;
    assert_eq!(seen.load(Ordering::SeqCst), 4, "and the actor lives on");
    assert!(handle.sys().remove("count").await.unwrap());
    handle.send(Msg::Add).await.unwrap();
    settled(&handle).await;
    assert_eq!(
        seen.load(Ordering::SeqCst),
        4,
        "a removed function sees nothing"
    );

    // A function installed under a taken id replaces the old one.
    let (old, new) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    for counter in [old.clone(), new.clone()] {
        handle
            .sys()
            .install("same", move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Install::Keep
            })
            .await
            .unwrap();
    }
    handle.send(Msg::Add).await.unwrap();
    settled(&handle).await;
    assert_eq!(
        (old.load(Ordering::SeqCst), new.load(Ordering::SeqCst)),
        (0, 2)
    );

    // A function that asks to be removed is removed after that entry.
    let once = Arc::new(AtomicUsize::new(0));
    let counter = once.clone();
    handle
        .sys()
        .install("once", move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Install::Remove
        })
        .await
        .unwrap();
    handle.send(Msg::Add).await.unwrap();
    assert_eq!(once.load(Ordering::SeqCst), 1);
    assert!(!handle.sys().remove("once").await.unwrap());

    // The captured state of a function is user code: its panic on drop stays
    // out of the actor.
    let guard = PanicOnDrop;
    handle
        .sys()
        .install("guarded", move |_| {
            let _keep = &guard;
            Install::Keep
        })
        .await
        .unwrap();
    assert!(handle.sys().remove("guarded").await.unwrap());
    assert_eq!(handle.send(Msg::Add).await.unwrap(), 1);

    // The message is shown before its handler runs and the reply after.
    handle.sys().remove("same").await.unwrap();
    let order = notes.clone();
    handle
        .sys()
        .install("order", move |entry| {
            match entry.event {
                DebugEvent::MessageIn { .. } => order.lock().unwrap().push("in"),
                DebugEvent::MessageOut { .. } => order.lock().unwrap().push("out"),
                DebugEvent::Stopping(_) => {}
            }
            Install::Keep
        })
        .await
        .unwrap();
    handle.send(Msg::Note).await.unwrap();
    settled(&handle).await;
    assert_eq!(*notes.lock().unwrap(), vec!["in", "handle", "out"]);
}

#[test]
fn installed_functions_on_both_flavors() {
    both_flavors(installed_functions_run_in_order_and_one_that_fails_leaves_the_others);
}

// ---------------------------------------------------------------------------
// Trace
// ---------------------------------------------------------------------------

type Traced = Arc<Mutex<Vec<DebugEntry>>>;

fn attach_trace(system: &ActorSystem, id: &str, kinds: KindSet) -> (Traced, Arc<AtomicUsize>) {
    let traced: Traced = Arc::default();
    let finished = Arc::new(AtomicUsize::new(0));
    let (sink, count) = (traced.clone(), finished.clone());
    assert!(
        system.attach_telemetry(id, kinds, move |event| match event {
            TelemetryEvent::Trace { entry, .. } => sink.lock().unwrap().push(entry.clone()),
            TelemetryEvent::TurnFinished { .. } => {
                count.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        })
    );
    (traced, finished)
}

async fn a_trace_reaches_a_handler_for_each_entry_of_a_turn_that_returned() {
    let system = system();
    let (traced, _) = attach_trace(&system, "trace", KindSet::of(&[EventKind::Trace]));
    let handle = spawn(&system, Probe::new(), DebugOptions::default().trace(true)).await;
    handle.send(Msg::Add).await.unwrap();
    handle.notify(Msg::Add).await.unwrap();
    handle.send(Msg::Add).await.unwrap();
    wait_for("the entries of three turns", || {
        traced.lock().unwrap().len() >= 5
    })
    .await;
    let events: Vec<DebugEvent> = traced
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.event.clone())
        .collect();
    let message_in = |reply_expected| DebugEvent::MessageIn {
        reply_expected,
        priority: false,
    };
    let out = DebugEvent::MessageOut { delivered: true };
    assert_eq!(
        events,
        vec![
            message_in(true),
            out.clone(),
            message_in(false),
            message_in(true),
            out
        ]
    );
    assert_in_order(&traced.lock().unwrap());

    // Off again: later turns raise nothing.
    handle.sys().set_trace(false).await.unwrap();
    handle.send(Msg::Add).await.unwrap();
    handle.sys().statistics().await.unwrap();
    assert_eq!(traced.lock().unwrap().len(), 5);

    // A turn that fails raises nothing: the actor is ending.
    handle.sys().set_trace(true).await.unwrap();
    assert!(handle.send(Msg::Fail).await.is_err());
    handle.wait_stopped().await;
    assert_eq!(traced.lock().unwrap().len(), 5);
}

#[test]
fn a_trace_reaches_a_handler_on_both_flavors() {
    both_flavors(a_trace_reaches_a_handler_for_each_entry_of_a_turn_that_returned);
}

async fn a_trace_with_the_trace_off_or_the_class_unattached_raises_nothing() {
    let system = system();
    // The handler takes turn events but not the trace.
    let (traced, finished) =
        attach_trace(&system, "turns", KindSet::of(&[EventKind::TurnFinished]));
    let on = spawn(&system, Probe::new(), DebugOptions::default().trace(true)).await;
    let off = spawn(
        &system,
        Probe::new(),
        DebugOptions::default().statistics(true),
    )
    .await;
    on.send(Msg::Add).await.unwrap();
    off.send(Msg::Add).await.unwrap();
    wait_for("both turns", || finished.load(Ordering::SeqCst) >= 2).await;
    on.sys().statistics().await.unwrap();
    assert!(traced.lock().unwrap().is_empty());

    // The class attached, the actor's trace off: still nothing.
    system.detach_telemetry("turns");
    let (traced, _) = attach_trace(&system, "trace", KindSet::of(&[EventKind::Trace]));
    off.send(Msg::Add).await.unwrap();
    off.sys().statistics().await.unwrap();
    assert!(traced.lock().unwrap().is_empty());
}

#[test]
fn a_trace_needs_the_trace_on_and_the_class_attached_on_both_flavors() {
    both_flavors(a_trace_with_the_trace_off_or_the_class_unattached_raises_nothing);
}
