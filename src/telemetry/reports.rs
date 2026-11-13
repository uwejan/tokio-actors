//! Crash, supervisor and progress reports at the levels of Erlang/OTP's
//! logger. A report is a log line of the `tracing` feature and nothing else:
//! it raises no event, runs no handler, and is formatted only when a
//! subscriber takes its target and level. The `format_status` call a crash
//! report makes is a turn of kind `Sys` like any other.
//!
//! * `tokio_actors::crash`, error: an actor that ends with a failure or with
//!   its supervisor's death (`proc_lib` crash report, `gen_server`
//!   termination report). A killed actor runs no code and is not reported.
//! * `tokio_actors::supervisor`, error, field `report`: `child_terminated`
//!   (supervisor.erl `do_restart`), `start_error` (a child that fails to
//!   start), `reached_max_restart_intensity`, and `shutdown_error`.
//! * `tokio_actors::supervisor`, field `report` `progress`: info for every
//!   automatic restart, debug for a `spawn_child` or `restart_child` that
//!   succeeds (supervisor.erl `progress` reports).

use crate::actor::subtree::{AbortRecord, SYS_SENSITIVE};
use crate::actor::sys::DebugEntry;
#[cfg(feature = "tracing")]
use crate::actor::sys::DebugEvent;
use crate::actor::Actor;
use crate::types::{ActorId, RestartType, StopReason, TurnClock};

/// The target of a crash report.
#[allow(dead_code)]
pub(crate) const CRASH_TARGET: &str = "tokio_actors::crash";

/// The target of the supervisor's reports.
#[allow(dead_code)]
pub(crate) const SUPERVISOR_TARGET: &str = "tokio_actors::supervisor";

/// What the report of one crash holds: the actor's formatted state and the
/// log's entries.
#[derive(Debug)]
pub(crate) struct CrashText {
    pub(crate) state: Option<String>,
    pub(crate) log: Vec<DebugEntry>,
    /// Whether the actor is sensitive: the log's entries print no reason.
    pub(crate) sensitive: bool,
}

/// Whether an actor that ended with `reason` gets a crash report: a failure,
/// or the end its supervisor's death ordered. A `Kill` runs no code of the
/// actor's, and the reasons of an orderly end are no crash.
#[inline]
pub(crate) fn crash_reported(reason: &StopReason) -> bool {
    matches!(reason, StopReason::Failure(_) | StopReason::ParentKilled)
}

/// The text of the report of an actor that ended with `reason`, or `None`
/// when the end gets no report or no subscriber takes it. Called on the
/// actor's task before its value is dropped: the one user code it runs is the
/// actor's `format_status`, once, bracketed as a turn and caught, and never
/// for a sensitive actor or an end that gets no report.
#[inline]
pub(crate) fn crash_text<A: Actor>(
    actor: Option<&A>,
    reason: &StopReason,
    record: &AbortRecord,
    ring: Option<&[DebugEntry]>,
    clock: &TurnClock,
) -> Option<CrashText> {
    if !crash_reported(reason) {
        return None;
    }
    crash_text_reported(actor, record, ring, clock)
}

/// The rest of [`crash_text`] for an end that gets a report: kept out of
/// line, so an orderly end tests the reason and goes on.
#[inline(never)]
fn crash_text_reported<A: Actor>(
    actor: Option<&A>,
    record: &AbortRecord,
    ring: Option<&[DebugEntry]>,
    clock: &TurnClock,
) -> Option<CrashText> {
    if !caught_log!(@enabled ERROR, CRASH_TARGET) {
        return None;
    }
    let sensitive = record.sys_bits() & SYS_SENSITIVE != 0;
    let state = actor.and_then(|actor| {
        crate::actor::sys_state::format_status_safely(actor, sensitive, record, clock)
    });
    Some(CrashText {
        state,
        log: ring.map(<[DebugEntry]>::to_vec).unwrap_or_default(),
        sensitive,
    })
}

/// Writes the report made by [`crash_text`], once the actor's end is
/// published. `fields` are the identity fields of the incarnation and `reason`
/// the reason it ended with.
#[cfg(feature = "tracing")]
#[inline]
pub(crate) fn crash_log(
    crash: Option<CrashText>,
    actor_id: &ActorId,
    fields: &crate::telemetry::LogFields<'_>,
    reason: &StopReason,
) {
    if let Some(crash) = crash {
        write_crash_log(crash, actor_id, fields, reason);
    }
}

/// The line of a crash report, out of line: only an end that was reported
/// reaches it.
#[cfg(feature = "tracing")]
#[inline(never)]
fn write_crash_log(
    crash: CrashText,
    actor_id: &ActorId,
    fields: &crate::telemetry::LogFields<'_>,
    reason: &StopReason,
) {
    let fields = fields.clone().with_sensitive(crash.sensitive);
    caught_log!(error,
        target: "tokio_actors::crash",
        report = "crash_report",
        actor_id = %actor_id,
        actor_name = fields.name(),
        incarnation = fields.incarnation(),
        system = fields.system(),
        reason = %fields.reason_text(reason),
        state = crash.state.as_deref(),
        log = %render_log(&crash.log, crash.sensitive),
        "the actor crashed"
    );
}

/// Without the `tracing` feature no report is made.
#[cfg(not(feature = "tracing"))]
#[inline]
pub(crate) fn crash_log(crash: Option<CrashText>, actor_id: &ActorId) {
    let _ = (crash, actor_id);
}

/// The log's entries as one line: each entry's number and what happened, the
/// reason of a stop left out for a sensitive actor.
#[cfg(feature = "tracing")]
fn render_log(entries: &[DebugEntry], hide_reason: bool) -> String {
    let mut out = String::new();
    for (at, entry) in entries.iter().enumerate() {
        if at > 0 {
            out.push_str("; ");
        }
        match (&entry.event, hide_reason) {
            (DebugEvent::Stopping(_), true) => out.push_str(&format!("{} Stopping", entry.seq)),
            (event, _) => out.push_str(&format!("{} {:?}", entry.seq, event)),
        }
    }
    out
}

/// Whether the end of a child of `restart_type` that ended with `reason` is
/// reported by its supervisor as `child_terminated`. Erlang/OTP reports every
/// exit of a permanent child, and an abnormal one of a transient or temporary
/// child (supervisor.erl `do_restart`). An exit is abnormal when
/// [`StopReason::is_normal`] does not call it clean, the one reading the
/// supervisor's restart decision makes, so a transient child is reported
/// exactly when that decision restarts it.
pub(crate) fn child_terminated_reported(restart_type: RestartType, reason: &StopReason) -> bool {
    match restart_type {
        RestartType::Permanent => true,
        RestartType::Transient | RestartType::Temporary => !reason.is_normal(),
    }
}

/// The report of a child's end that its supervisor evaluates for a restart.
#[allow(unused_variables)]
pub(crate) fn child_terminated(
    supervisor: &ActorId,
    child: &ActorId,
    incarnation: u64,
    restart_type: RestartType,
    reason: &StopReason,
    sensitive: bool,
) {
    #[cfg(feature = "tracing")]
    if child_terminated_reported(restart_type, reason)
        && caught_log!(@enabled ERROR, SUPERVISOR_TARGET)
    {
        let fields = crate::telemetry::LogFields::of_sensitive(sensitive);
        caught_log!(error,
            target: "tokio_actors::supervisor",
            report = "child_terminated",
            supervisor = %supervisor,
            child = %child,
            incarnation,
            restart_type = ?restart_type,
            reason = %fields.reason_text(reason),
            "a supervised child terminated"
        );
    }
}

/// The report of a child that failed to start, as a restart attempt or as a
/// `spawn_child`. `reason` is built only when a subscriber takes the report.
#[allow(unused_variables)]
pub(crate) fn start_error(
    supervisor: &ActorId,
    child: &ActorId,
    incarnation: Option<u64>,
    reason: impl FnOnce() -> StopReason,
    sensitive: bool,
) {
    #[cfg(feature = "tracing")]
    if caught_log!(@enabled ERROR, SUPERVISOR_TARGET) {
        let fields = crate::telemetry::LogFields::of_sensitive(sensitive);
        let reason = reason();
        caught_log!(error,
            target: "tokio_actors::supervisor",
            report = "start_error",
            supervisor = %supervisor,
            child = %child,
            incarnation,
            reason = %fields.reason_text(&reason),
            "a supervised child failed to start"
        );
    }
}

/// The report of a supervisor whose restart budget the charge of `child`'s
/// death exhausted. Called once, at that charge.
#[allow(unused_variables)]
pub(crate) fn reached_max_restart_intensity(
    supervisor: &ActorId,
    child: &ActorId,
    incarnation: u64,
    max: u32,
    window: std::time::Duration,
) {
    #[cfg(feature = "tracing")]
    if caught_log!(@enabled ERROR, SUPERVISOR_TARGET) {
        caught_log!(error,
            target: "tokio_actors::supervisor",
            report = "reached_max_restart_intensity",
            supervisor = %supervisor,
            child = %child,
            incarnation,
            max_restarts = max,
            window_ms = window.as_millis() as u64,
            "the supervisor reached its maximum restart intensity and stops"
        );
    }
}

/// What a progress report records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// A restart the supervisor decided (info).
    Restarted,
    /// A child the caller started or restarted by hand (debug).
    Started,
}

/// The progress report of a child that started under `supervisor`.
#[allow(unused_variables)]
pub(crate) fn progress(supervisor: &ActorId, child: &ActorId, incarnation: u64, kind: Progress) {
    #[cfg(feature = "tracing")]
    match kind {
        Progress::Restarted => {
            if caught_log!(@enabled INFO, SUPERVISOR_TARGET) {
                caught_log!(info,
                    target: "tokio_actors::supervisor",
                    report = "progress",
                    supervisor = %supervisor,
                    child = %child,
                    incarnation,
                    "a supervised child was restarted"
                );
            }
        }
        Progress::Started => {
            if caught_log!(@enabled DEBUG, SUPERVISOR_TARGET) {
                caught_log!(debug,
                    target: "tokio_actors::supervisor",
                    report = "progress",
                    supervisor = %supervisor,
                    child = %child,
                    incarnation,
                    "a supervised child was started"
                );
            }
        }
    }
}

#[cfg(test)]
#[cfg(feature = "tracing")]
mod tests {
    use super::*;
    use crate::actor::sys::DebugEvent;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Metadata, Subscriber};

    /// Keeps the `log` and `reason` fields of every event.
    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<(String, String)>>>);

    struct Fields(Vec<(String, String)>);

    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }

    impl Subscriber for Lines {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut fields = Fields(Vec::new());
            event.record(&mut fields);
            let mut kept = self.0.lock().unwrap();
            for (name, value) in fields.0 {
                if name == "log" || name == "reason" {
                    kept.push((name, value));
                }
            }
        }
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    fn entries() -> Vec<DebugEntry> {
        vec![
            DebugEntry {
                seq: 1,
                event: DebugEvent::MessageIn {
                    reply_expected: false,
                    priority: false,
                },
            },
            DebugEntry {
                seq: 2,
                event: DebugEvent::Stopping(StopReason::Failure(crate::ActorError::user("hidden"))),
            },
        ]
    }

    fn written(sensitive: bool) -> Vec<(String, String)> {
        let lines = Lines::default();
        {
            let _installed = crate::tracing_isolation::install(lines.clone());
            let crash = CrashText {
                state: None,
                log: entries(),
                sensitive,
            };
            let fields = crate::telemetry::LogFields::default();
            let reason = StopReason::Failure(crate::ActorError::user("hidden"));
            crash_log(Some(crash), &ActorId::from("a"), &fields, &reason);
        }
        let kept = lines.0.lock().unwrap().clone();
        kept
    }
}
