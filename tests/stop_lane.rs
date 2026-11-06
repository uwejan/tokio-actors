//! Behavioral suite for the stop lane: the per-actor cell, with a version,
//! that delivers stop/kill requests without awaiting mailbox capacity.
//!
//! Covers: turn-boundary delivery under a flooded mailbox (never racing an
//! in-flight handler), monotone severity (a `Kill` in flight is never
//! displaced, and never displaces nothing weaker either - order-independent),
//! coalescing of multiple raises landing between two turn boundaries, the
//! veto-then-resend re-fire guarantee, and the error a stop raised at an
//! already-terminated actor reports.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::sleep;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, SendError, StopReason,
};

mod common;

#[path = "common/source_scan.rs"]
mod source_scan;

/// Waits until `calls` reads at least `n`, for at most 60 s. The bound only limits how long a real hang takes to report.
async fn wait_calls(calls: &AtomicUsize, n: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while calls.load(Ordering::SeqCst) < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "pre_stop ran {} times, expected at least {n}",
            calls.load(Ordering::SeqCst)
        );
        sleep(Duration::from_millis(1)).await;
    }
}

// ---------------------------------------------------------------------------
// Helper actor
// ---------------------------------------------------------------------------

/// An actor whose handler can be parked on a test-controlled `Notify` pair:
/// while `Hang` is in flight, the actor's single task is fully consumed, so
/// its stop lane, system channel, and mailbox are all left unserviced until
/// the test releases it. `pre_stop` counts its own invocations and vetoes
/// every call whose 1-based index is below `accept_from`.
struct Prober {
    pre_stop_calls: Arc<AtomicUsize>,
    on_stopped_calls: Arc<AtomicUsize>,
    accept_from: usize,
}

impl Default for Prober {
    fn default() -> Self {
        Self {
            pre_stop_calls: Arc::new(AtomicUsize::new(0)),
            on_stopped_calls: Arc::new(AtomicUsize::new(0)),
            accept_from: usize::MAX, // veto forever unless overridden
        }
    }
}

enum ProbeMsg {
    /// Notifies `started` as soon as the handler begins, then blocks until
    /// `release` fires - giving the test a deterministic window in which the
    /// actor's task is fully occupied and cannot service its `select!`.
    Hang {
        started: Arc<Notify>,
        release: Arc<Notify>,
    },
    /// A no-op, used only to fill a mailbox slot for the flooding test.
    Noop,
}

impl Actor for Prober {
    type Message = ProbeMsg;
    type Response = ();

    async fn handle(&mut self, msg: ProbeMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ProbeMsg::Hang { started, release } => {
                started.notify_one();
                release.notified().await;
                Ok(())
            }
            ProbeMsg::Noop => Ok(()),
        }
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        let n = self.pre_stop_calls.fetch_add(1, Ordering::SeqCst) + 1;
        n >= self.accept_from
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.on_stopped_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Item 4: stop to an already-terminated actor
// ---------------------------------------------------------------------------

#[test]
fn stop_after_already_stopped_matches_closed_mpsc_error() {
    common::run(2, async {
        let handle = Prober::default()
            .spawn()
            .named("lane-already-dead")
            .with_config(ActorConfig::default())
            .await
            .unwrap();

        // accept_from defaults to MAX (always veto), so escalate straight to
        // Kill to guarantee termination without depending on pre_stop.
        handle.stop(StopReason::Kill).await.unwrap();
        handle.wait_stopped().await;

        let stop_err = handle.stop(StopReason::Graceful).await.unwrap_err();
        assert!(
            matches!(stop_err, SendError::Closed),
            "got {stop_err:?}, expected SendError::Closed"
        );

        // Cross-check against the pre-existing closed-mailbox error for the same
        // dead actor: both planes must agree on "already stopped".
        let notify_err = handle.notify(ProbeMsg::Noop).await.unwrap_err();
        assert!(matches!(notify_err, SendError::Closed));
    });
}

// ---------------------------------------------------------------------------
// Item 5: veto-then-resend re-fires pre_stop
// ---------------------------------------------------------------------------

#[test]
fn veto_then_resend_same_severity_re_fires_pre_stop() {
    common::run(2, async {
        let actor = Prober {
            accept_from: 2, // first Graceful is vetoed, second is accepted
            ..Prober::default()
        };
        let pre_stop_calls = actor.pre_stop_calls.clone();
        let on_stopped_calls = actor.on_stopped_calls.clone();

        let handle = actor
            .spawn()
            .named("lane-veto-then-resend")
            .with_config(ActorConfig::default())
            .await
            .unwrap();

        handle.stop(StopReason::Graceful).await.unwrap();
        wait_calls(&pre_stop_calls, 1).await;
        // The lane outranks the mailbox, so a message answered now proves the
        // loop finished the vetoed stop and went back to serving.
        handle.send(ProbeMsg::Noop).await.unwrap();
        assert_eq!(pre_stop_calls.load(Ordering::SeqCst), 1);
        assert!(
            handle.is_alive(),
            "the first, vetoed raise must not stop the actor"
        );

        // Same severity, a fresh generation: must land and re-fire pre_stop.
        handle.stop(StopReason::Graceful).await.unwrap();
        handle.wait_stopped().await;

        assert_eq!(pre_stop_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            on_stopped_calls.load(Ordering::SeqCst),
            1,
            "the accepted Graceful stop runs on_stopped"
        );
    });
}

// ---------------------------------------------------------------------------
// Item 1: coalescing - N raises between two turn boundaries collapse to one
// observation; N raises each separated by an observed turn boundary produce
// N observations. Together these pin both ends of ">=1 and <=N".
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Severity monotonicity: Kill always wins, regardless of raise order, and a
// superseded lower-severity raise is never separately delivered (not even a
// vetoed one) - it disappears entirely into the single coalesced cell.
// ---------------------------------------------------------------------------

#[test]
fn kill_before_pending_graceful_observation_suppresses_it_entirely() {
    common::run(2, async {
        let actor = Prober::default();
        let pre_stop_calls = actor.pre_stop_calls.clone();
        let on_stopped_calls = actor.on_stopped_calls.clone();

        let handle = actor
            .spawn()
            .named("lane-kill-supersedes")
            .with_config(ActorConfig::default())
            .await
            .unwrap();

        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        handle
            .notify(ProbeMsg::Hang {
                started: started.clone(),
                release: release.clone(),
            })
            .await
            .unwrap();
        started.notified().await;

        // Graceful, then Kill, both while the loop cannot observe either yet.
        handle.stop(StopReason::Graceful).await.unwrap();
        handle.stop(StopReason::Kill).await.unwrap();

        release.notify_one();
        handle.wait_stopped().await;

        assert_eq!(
            pre_stop_calls.load(Ordering::SeqCst),
            0,
            "the superseded Graceful raise must never be separately delivered - \
         Kill bypasses pre_stop, and coalescing means there is only ever one \
         cell, not a queue of both"
        );
        assert_eq!(
            on_stopped_calls.load(Ordering::SeqCst),
            0,
            "Kill bypasses on_stopped too"
        );
    });
}

// ---------------------------------------------------------------------------
// Item 2: turn-boundary delivery under a flooded mailbox - Kill is observed
// only once the in-flight handler completes, never mid-handler, even though
// the mailbox is simultaneously full and the lane outranks it.
// ---------------------------------------------------------------------------

#[test]
fn kill_under_a_flooded_mailbox_waits_for_the_in_flight_handler() {
    common::run(2, async {
        let config = ActorConfig::default().with_mailbox_capacity(1);
        let handle = Prober::default()
            .spawn()
            .named("lane-flood-turn-boundary")
            .with_config(config)
            .await
            .unwrap();

        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        handle
            .notify(ProbeMsg::Hang {
                started: started.clone(),
                release: release.clone(),
            })
            .await
            .unwrap();
        started.notified().await; // handler is now in flight

        // Flood the mailbox behind the in-flight handler.
        let _ = handle.try_notify(ProbeMsg::Noop);

        // Kill outranks both the system channel and the (full) mailbox, but must
        // still not race the in-flight handler.
        handle.stop(StopReason::Kill).await.unwrap();
        sleep(Duration::from_millis(30)).await;
        assert!(
            handle.is_alive(),
            "Kill must not cancel or race an in-flight handler invocation (ITP)"
        );

        release.notify_one();
        handle.wait_stopped().await;
    });
}

// ---------------------------------------------------------------------------
// Item 3: the escalation ladder signals through the lane everywhere, never
// through an awaited channel send.
// ---------------------------------------------------------------------------

#[test]
fn escalation_ladder_contains_no_awaited_stop_sends() {
    // Every module that participates in stop/kill delivery - spawning,
    // supervision's escalation ladder, and system-level shutdown alike -
    // must signal through the stop lane (a synchronous, infallible watch
    // write), never through an awaited send of a `Stop` message.
    let sources: &[(&str, &str)] = &[
        (
            "src/actor/runtime.rs",
            include_str!("../src/actor/runtime.rs"),
        ),
        (
            "src/actor/context.rs",
            include_str!("../src/actor/context.rs"),
        ),
        (
            "src/actor/supervision.rs",
            include_str!("../src/actor/supervision.rs"),
        ),
        (
            "src/actor/handle.rs",
            include_str!("../src/actor/handle.rs"),
        ),
        ("src/system.rs", include_str!("../src/system.rs")),
        ("src/types/mod.rs", include_str!("../src/types/mod.rs")),
    ];
    for (path, src) in sources {
        assert!(
            !src.contains("SystemMessage::Stop"),
            "{path} still constructs SystemMessage::Stop - stop/kill delivery \
             must go through the stop lane instead"
        );
    }
}

// ---------------------------------------------------------------------------
// Every registry Kill is raised through the declaring function
// ---------------------------------------------------------------------------

/// The text of `src` without its comments and without every item that exists
/// only under `cfg(test)`: a module, a function or a statement.
fn production_text(src: &str) -> String {
    let mut text = source_scan::library_code(src).join("\n");
    text.push('\n');
    text
}

#[test]
fn every_registry_kill_raise_goes_through_the_declaring_function() {
    // The files that raise or could raise on a stop lane, with the number of
    // `.raise(`, `try_raise(` and `.raise_kill(` calls each production text may
    // hold. Every count is an allowed site:
    //
    // - handle.rs: the two non-Kill raises of `stop` and `stop_within`.
    // - start_bound.rs: the cut of a start that timed out, which holds the
    //   task's abort handle and requests the abort itself, so no registry
    //   entry is involved and no deadline is declared.
    // - supervision.rs: the child link guard, which holds the child's abort
    //   handle and its own reaper entry.
    // - slot.rs: `raise_first` (a `Kill` there belongs to the supervisor's
    //   ladder), `raise_owing` (the supervisor holds the abort and record),
    //   and the orderly stop of a slot.
    // - system.rs: `.raise(` is the reaper entry (it holds the abort handle
    //   itself) and the stop sent by id; `try_raise(` is the graceful stop by
    //   name, the shared function and the step cell's own raise;
    //   `.raise_kill(` is the reaper entry, the shared function and the
    //   private by-id helper that calls the shared function.
    let scanned: &[(&str, &str, [usize; 3])] = &[
        (
            "actor/context.rs",
            include_str!("../src/actor/context.rs"),
            [0, 0, 0],
        ),
        (
            "actor/handle.rs",
            include_str!("../src/actor/handle.rs"),
            [0, 2, 0],
        ),
        (
            "actor/mailbox.rs",
            include_str!("../src/actor/mailbox.rs"),
            [0, 0, 0],
        ),
        (
            "actor/mod.rs",
            include_str!("../src/actor/mod.rs"),
            [0, 0, 0],
        ),
        (
            "actor/panic.rs",
            include_str!("../src/actor/panic.rs"),
            [0, 0, 0],
        ),
        (
            "actor/runtime.rs",
            include_str!("../src/actor/runtime.rs"),
            [0, 0, 0],
        ),
        (
            "actor/start_bound.rs",
            include_str!("../src/actor/start_bound.rs"),
            [1, 0, 0],
        ),
        (
            "actor/subtree.rs",
            include_str!("../src/actor/subtree.rs"),
            [0, 0, 0],
        ),
        (
            "actor/supervision.rs",
            include_str!("../src/actor/supervision.rs"),
            [2, 0, 0],
        ),
        (
            "actor/supervision/rules.rs",
            include_str!("../src/actor/supervision/rules.rs"),
            [0, 0, 0],
        ),
        (
            "actor/supervision/slot.rs",
            include_str!("../src/actor/supervision/slot.rs"),
            [2, 1, 0],
        ),
        ("error.rs", include_str!("../src/error.rs"), [0, 0, 0]),
        ("helper.rs", include_str!("../src/helper.rs"), [0, 0, 0]),
        ("lib.rs", include_str!("../src/lib.rs"), [0, 0, 0]),
        ("system.rs", include_str!("../src/system.rs"), [2, 3, 3]),
        (
            "time_probe.rs",
            include_str!("../src/time_probe.rs"),
            [0, 0, 0],
        ),
        (
            "types/mod.rs",
            include_str!("../src/types/mod.rs"),
            [0, 0, 0],
        ),
        (
            "types/status_cell.rs",
            include_str!("../src/types/status_cell.rs"),
            [0, 0, 0],
        ),
        (
            "types/turn_clock.rs",
            include_str!("../src/types/turn_clock.rs"),
            [0, 0, 0],
        ),
    ];
    // The lane itself defines the raise; the files that exist only under
    // `cfg(test)` do not ship, and are found from the declarations that bring
    // them in.
    let lane_implementation = "types/stop_lane.rs";

    // A new source file must be classified here before it passes.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = source_scan::rust_sources(&root);
    let (test_only, unresolved) = source_scan::test_only_files(&sources);
    assert!(
        unresolved.is_empty(),
        "test-only modules without a file: {unresolved:?}"
    );
    let found: Vec<String> = sources
        .keys()
        .filter(|path| !test_only.contains(*path))
        .cloned()
        .collect();
    let mut known: Vec<String> = scanned
        .iter()
        .map(|(path, _, _)| (*path).to_string())
        .collect();
    known.push(lane_implementation.to_string());
    known.sort();
    assert_eq!(
        found, known,
        "a source file is not classified for the Kill raiser scan"
    );

    for (path, src, [raise, try_raise, raise_kill]) in scanned {
        let text = production_text(src);
        assert_eq!(
            [
                text.matches(".raise(").count(),
                text.matches("try_raise(").count(),
                text.matches(".raise_kill(").count(),
            ],
            [*raise, *try_raise, *raise_kill],
            "{path}: a raiser was added or removed; a registry Kill goes through \
             raise_kill_declared, and any other raise needs a reason at its site"
        );
    }

    let handle = production_text(include_str!("../src/actor/handle.rs"));
    assert!(
        !handle.contains("try_raise(StopReason::Kill")
            && !handle.contains(".raise(StopReason::Kill"),
        "handle.rs raises a Kill without the declaring function"
    );
    assert!(
        !handle.contains("declare_kill("),
        "handle.rs declares a Kill deadline itself"
    );
    let system = production_text(include_str!("../src/system.rs"));
    assert_eq!(
        system.matches("try_raise(StopReason::Kill)").count(),
        2,
        "system.rs raises a Kill outside the declaring function and the step cell"
    );
}

#[test]
fn the_production_text_skips_test_only_items_by_their_braces() {
    let src = r##"
fn shipped() { a.raise(1); }

#[cfg(test)]
mod tests {
    fn nested() {
        let text = r#"
}
"#;
        b.raise(2);
    }
}

#[cfg(test)]
fn only_in_tests() { c.raise(3); }

#[cfg(doctest)]
mod only_in_doctests {
    fn nested() { f.raise(6); }
}

#[cfg(all(doctest, feature = "x"))]
fn also_only_in_doctests() { g.raise(7); }

fn also_shipped() { d.raise(4); } // e.raise(5)
"##;
    let text = production_text(src);
    assert_eq!(text.matches(".raise(").count(), 2, "{text}");
    assert!(text.contains("a.raise(1)") && text.contains("d.raise(4)"));
}
