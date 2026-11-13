//! Where the library emits telemetry. Every event outside the telemetry
//! module goes out through the push function of the system's gate, which
//! queues it for the pump's helper: no handler runs on the path that raised
//! the event, so none runs where the crate still owes a step. The dispatch
//! function, which runs the handlers on the calling thread, is called only
//! inside the telemetry module, and never from the reaper, the sweeps, a
//! helper's end guard, the stop lane, the mailbox or any `Drop`. The gate
//! itself is read through the system's telemetry state, which the incarnation's record holds, and costs two relaxed loads, so
//! its functions look nothing up by name and clone nothing. The two calls that
//! run handlers on the calling task, the cached one for hot events and the
//! locked one, are made only from four listed functions, each of which owes
//! nothing after the call, and never from a drop, a helper or the reaper. A
//! call that breaks one of these rules fails this test with the file and
//! function.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[path = "common/source_scan.rs"]
mod source_scan;

use source_scan::{fn_bodies, library_code, rust_sources, test_only_files};

/// The macros that write a log line. `helper_log!` expands to `caught_log!`
/// with the caller's fields in one argument, so each call of either is read.
const LOG_MACROS: &[&str] = &["caught_log", "helper_log"];

/// The tracing macros that write an event. A line that calls one directly
/// bypasses the log macros, so no library line may.
const TRACING_EVENTS: &[&str] = &["error", "warn", "info", "debug", "trace", "event"];

/// The log fields that print text derived from an actor's failure or panic.
const USER_TEXT_FIELDS: &[&str] = &["panic", "error", "reason", "payload"];

/// The number of such fields per file that still print the text itself and
/// not what a `LogFields` method returns. A file not listed has none; a field
/// outside this list fails the test, and a count that is too high fails it
/// too, so the list only shrinks. It is empty: every such field redacts.
const USER_TEXT_UNREDACTED: &[(&str, usize)] = &[];

/// The `LogFields` methods that hide the text of a sensitive actor's failure:
/// an error's text, a panic's, and a stop reason's.
const REDACTING_CALLS: &[&str] = &[".text(", ".panic_text(", ".reason_text("];

/// The files that print text through `LogFields::foreign()`, with the number
/// of calls in each: a telemetry handler's panic, a caller's waker's panic and
/// the panic of a helper that works for no actor belong to no actor's failure,
/// so nothing there is hidden. Every line about an actor's own panic, error or
/// stop reason names the actor's sensitivity.
const FOREIGN_TEXT_CALLS: &[(&str, usize)] = &[
    ("telemetry/handlers.rs", 2),
    ("actor/panic.rs", 2),
    ("helper.rs", 1),
];

/// The files that may build log fields with `LogFields::unattributed()`, which
/// knows no actor and hides nothing: none, and the constructor is gone.
const UNATTRIBUTED_CALLS: &[(&str, usize)] = &[];

/// Whether `file` holds code of the `sys` bundle, the system monitor or the
/// reports: none of it raises a hot event, dispatches, or keeps state of the
/// process or the thread.
fn is_sys_file(file: &str) -> bool {
    (file.starts_with("actor/sys") && file.ends_with(".rs"))
        || file == "telemetry/monitor.rs"
        || file == "telemetry/reports.rs"
}

/// The arguments of the calls in `lines` that begin with one of `needles`
/// (each ends in the opening bracket), one list per call, split at the commas
/// outside brackets.
fn calls_of(lines: &[String], needles: &[&str]) -> Vec<Vec<String>> {
    let mut calls = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        let Some((start, needle)) = needles
            .iter()
            .filter_map(|needle| lines[at].find(needle).map(|start| (start, *needle)))
            .min_by_key(|(start, _)| *start)
        else {
            at += 1;
            continue;
        };
        let mut text = lines[at][start + needle.len()..].to_string();
        let mut depth = 1i32;
        let mut end = at;
        // The text before `scanned` has been read already.
        let mut scanned = 0;
        loop {
            let mut cut = None;
            for (i, c) in text[scanned..].char_indices() {
                match c {
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => {
                        depth -= 1;
                        if depth == 0 {
                            cut = Some(scanned + i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(i) = cut {
                text.truncate(i);
                break;
            }
            scanned = text.len();
            end += 1;
            if end >= lines.len() {
                break;
            }
            text.push(' ');
            text.push_str(&lines[end]);
        }
        let mut args = Vec::new();
        let (mut depth, mut from) = (0i32, 0usize);
        for (i, c) in text.char_indices() {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    args.push(text[from..i].trim().to_string());
                    from = i + 1;
                }
                _ => {}
            }
        }
        args.push(text[from..].trim().to_string());
        // A trailing comma ends the list; it adds no argument.
        if args.len() > 1 && args.last().is_some_and(String::is_empty) {
            args.pop();
        }
        calls.push(args);
        at = end.max(at) + 1;
    }
    calls
}

/// The arguments of the log macro calls in `lines`.
fn log_calls(lines: &[String]) -> Vec<Vec<String>> {
    let needles: Vec<String> = LOG_MACROS.iter().map(|name| format!("{name}!(")).collect();
    let needles: Vec<&str> = needles.iter().map(String::as_str).collect();
    calls_of(lines, &needles)
}

/// The fields of the log macro calls in `lines` that print user-derived text
/// with `%` or `?` and do not call a redacting method on it.
fn unredacted_fields(lines: &[String]) -> Vec<String> {
    let mut found = Vec::new();
    for args in log_calls(lines) {
        for arg in args {
            let Some((name, value)) = arg.split_once('=') else {
                continue;
            };
            let (name, value) = (name.trim(), value.trim());
            let printed = value.starts_with('%') || value.starts_with('?');
            if USER_TEXT_FIELDS.contains(&name)
                && printed
                && !REDACTING_CALLS.iter().any(|call| value.contains(call))
            {
                found.push(name.to_string());
            }
        }
    }
    found
}

/// The files that may call the dispatch function, besides the telemetry
/// module itself: none. Dispatch runs the handlers on the calling thread, and
/// a handler must never run on a path that still owes the crate a step (an
/// init ack, a mail drop, a status or verdict publication, a release), so
/// every site outside the telemetry module queues its event with `push`.
const DISPATCH_FILES: &[&str] = &[];

/// Functions that run on the reaper, the sweeps and the kill declarations,
/// where user code must never run.
const DENY_FNS: &[(&str, &str)] = &[
    ("system.rs", "fire"),
    ("system.rs", "fire_due"),
    ("system.rs", "reaper_run"),
    ("system.rs", "reaper_loop"),
    ("system.rs", "feed_reaper"),
    ("system.rs", "arm_kill_backstop"),
    ("system.rs", "arm_raised_kill"),
    ("system.rs", "raise_kill_declared"),
    ("system.rs", "raise_noted_engaged"),
    ("system.rs", "declare_kill"),
    ("system.rs", "declare_kill_by_name"),
    ("system.rs", "concurrent_force_sweep"),
];

/// Files in which no dispatch call may appear at all.
const DENY_FILES: &[&str] = &[
    "helper.rs",
    "actor/subtree.rs",
    "types/stop_lane.rs",
    "actor/mailbox.rs",
];

/// The calls that write a request to a stop lane.
const RAISE_CALLS: &[&str] = &[".try_raise(", ".raise("];

/// The call that queues the event of a stop request.
const REQUEST_EVENT: &str = "emit_stop_requested(";

/// Every function of the library that writes a request to a stop lane, with
/// what ties it to the request's event: `emits` when the function queues the
/// event itself, otherwise the reason its callers do. A raise anywhere else
/// goes through `raise_noted`, which queues the request and, for a `Kill`
/// the lane took, its `Raised` step. The lane's own file defines the raise
/// and is not scanned.
const RAISE_FNS: &[(&str, &str, &str)] = &[
    (
        "actor/handle.rs",
        "raise_request",
        "its caller `stop_noted` queues the event",
    ),
    ("actor/handle.rs", "stop_within", "emits"),
    ("system.rs", "stop", "emits"),
    ("system.rs", "raise_kill_declared", "emits"),
    (
        "system.rs",
        "raise_noted",
        "with no sink and no identity there is no event to queue",
    ),
    ("system.rs", "raise_noted_engaged", "emits"),
    ("system.rs", "fire", "emits"),
    (
        "system.rs",
        "raise_kill",
        "the step cell's own raise; its callers `raise_kill_declared` and `fire` queue the events",
    ),
];

/// The calls that abort a task through tokio: a join handle's or an abort
/// handle's.
const ABORT_CALLS: &[&str] = &[".abort()", "JoinHandle::abort(", "AbortHandle::abort("];

/// Every function of the library that aborts a task itself, with the reason
/// it may. An abort of an incarnation's task goes through the record's
/// `abort` (it writes the first instant and queues the `AbortFired` step
/// under the record's lock, ahead of every event the task's end raises), so
/// the one allowed site is that function.
const ABORT_FNS: &[(&str, &str, &str)] = &[(
    "actor/subtree.rs",
    "abort",
    "the record's abort: the one place that aborts an incarnation's task",
)];

/// The lane's own file.
const LANE_FILE: &str = "types/stop_lane.rs";

/// Files in which no synchronous emit may appear at all.
const NO_SYNC_FILES: &[&str] = &["helper.rs", "actor/subtree.rs", "types/stop_lane.rs"];

/// The calls of the dispatch function: a method call or a path call.
const DISPATCH_CALLS: &[&str] = &[".dispatch(", "::dispatch("];

/// The functions that may hand an event to the sinks, by the call they make:
/// the dispatch function, the pump's drain, and the sinks' own delivery.
const DELIVER_SITES: &[(&str, &[(&str, &str)])] = &[
    (
        "sinks.deliver(",
        &[
            ("telemetry/mode.rs", "dispatch"),
            ("telemetry/pump.rs", "drain"),
        ],
    ),
    ("handlers::run(", &[("telemetry/sinks.rs", "deliver")]),
    (
        "subscription::deliver(",
        &[("telemetry/sinks.rs", "deliver")],
    ),
];

/// The functions of the gate: read on every emit site, disarmed or not.
const HOT_FILES: &[&str] = &["telemetry/mode.rs"];
const HOT_FNS: &[&str] = &["armed", "dispatch", "push"];

/// What the gate's functions must not contain: a clone of anything, a clock,
/// an allocation, an id, or a lookup of a system by name.
const HOT_BANNED: &[&str] = &[
    ".clone(",
    "Instant::now",
    "SystemTime",
    ".to_string(",
    ".to_owned(",
    "String::from",
    "format!",
    "Arc::new",
    "Box::new",
    "ActorId",
    ".id()",
    "get_named",
    "get_by_id",
    "get_by_name",
];

/// What nothing in the telemetry module may use to find a system or an actor:
/// events carry their own references and the gate lives in the mode.
const LOOKUPS: &[&str] = &[
    "get_named",
    "get_by_id",
    "get_by_name",
    "ActorSystem::get(",
    "ActorSystem::default",
    "systems()",
    "SYSTEMS",
];

/// The telemetry files that keep a per-thread cell: the flag that marks a
/// thread inside a handler, and the caller token of the wait graph, which is
/// read only inside a hook poll.
const THREAD_LOCAL_FILES: &[&str] = &["telemetry/handlers.rs", "telemetry/token.rs"];

/// The calls that run handlers on the calling task: the cached one and the
/// locked one, as a method call or as a path call (`telemetry::emit_hot(`).
const HOT_EMIT_CALLS: &[&str] = &[".emit_hot(", ".emit_slow(", "::emit_hot(", "::emit_slow("];

/// The only functions that may make those calls, each with the reason it owes
/// nothing after the call.
const HOT_SYNC_SITES: &[(&str, &str, &str)] = &[
    (
        "actor/runtime.rs",
        "turn_done_hot",
        "the actor's own run loop after a handle turn that returned Continue and its reply was sent (the plain turn and the general turn while the loop runs; a turn after the loop ended, and a turn that stopped its own actor, queue on the pump instead): the loop owes only its next take; the waiters on this task are the stop ladder and a bounded stop's verdict (the stop lane is read at the turn boundary), a requester on a current_thread runtime, and the supervisor reading the end, and a blocking handler holds them, as the docs of attach_telemetry say",
    ),
    (
        "actor/runtime.rs",
        "run_actor",
        "the actor's own run loop where a run ends: the same task and the same waiters as dispatch, and nothing the crate must publish is owed at that point",
    ),
];

/// The calls that queue an event with no owner: the gate's plain push. An
/// event that names an actor goes through `push_owned`, `push_built_owned`,
/// `push_about`, `stage_about` or `push_for`, so that it raises the busy count
/// of the incarnation it names and a hot event of that actor, which runs
/// inline only while the count reads zero, never overtakes it.
const PLAIN_PUSH_CALLS: &[&str] = &[".push(EventKind::", ".push(kind,"];

/// The only functions that may queue an event with the plain push, each with
/// the reason its event has no incarnation to count it in.
const PLAIN_PUSH_SITES: &[(&str, &str, &str)] = &[
    (
        "actor/runtime.rs",
        "refused",
        "a refused spawn creates no incarnation: the event names a requested name and an error",
    ),
    (
        "helper.rs",
        "record",
        "the fallback of an end whose incarnation has no identity left to count it in; an end that finds the identity of its owner's record goes through the counting push, and a lookup by id alone is never made",
    ),
    (
        "telemetry/identity.rs",
        "push_for",
        "the one fallback for a subject the registry does not hold, whose reference is the unknown-incarnation root",
    ),
];

/// The calls that start a helper task through a system's slot: the system's
/// `spawn_helper`, and the slot's own `spawn`, which takes the same four
/// arguments (runtime, kind, owner, the task's constructor).
const HELPER_SPAWN_CALLS: &[&str] = &["spawn_helper(", ".spawn("];

/// Every function that starts a helper task, with the owner it passes and the
/// reason. A helper that works for an incarnation passes `Some` owner built
/// with that incarnation's record, so the end of the helper is counted in the
/// incarnation's events and a hot event of the actor never overtakes it; a
/// helper that works for no incarnation passes `None`, and `pass` hands the
/// caller's owner on. A spawn on neither list, or with the other shape,
/// fails.
const HELPER_SPAWN_SITES: &[(&str, &str, &str, &str)] = &[
    (
        "actor/handle.rs",
        "spawn_ladder",
        "Some",
        "the stop ladder works for the actor it stops",
    ),
    (
        "actor/start_bound.rs",
        "bounded_start",
        "Some",
        "the start bound works for the actor it starts",
    ),
    (
        "actor/runtime.rs",
        "end_queue_late",
        "Some",
        "the late drain works for the actor whose queue ended; the record is the one the queue's end was handed, absent only for a slot dropped by an abort or an unwind, which holds none",
    ),
    (
        "system.rs",
        "system_monitor",
        "Some",
        "the monitor's helper works for the actor the monitor reports to",
    ),
    (
        "actor/supervisor_ref.rs",
        "terminate",
        "Some",
        "the reply helper works for the supervisor whose child it waits for: its end is counted among the supervisor's events, through the supervisor's record",
    ),
    (
        "system.rs",
        "spawn_helper",
        "pass",
        "the system's entry hands its caller's owner to the slot",
    ),
    (
        "system.rs",
        "ensure_sweep_spawned_with",
        "None",
        "the turn-watchdog sweep works for the system, not for an actor",
    ),
    (
        "system.rs",
        "reaper_handle",
        "None",
        "the reaper works for the runtime's actors together, not for one",
    ),
    (
        "system.rs",
        "shutdown_with",
        "None",
        "the shutdown walk works for the system, and its end names no actor",
    ),
    (
        "telemetry/pump.rs",
        "ensure_started",
        "None",
        "the pump reports no end of its own and works for the system",
    ),
];

/// The library files by path with their library lines, test-only files left
/// out.
fn library_lines(sources: &BTreeMap<String, String>) -> BTreeMap<String, Vec<String>> {
    let (test_files, unresolved) = test_only_files(sources);
    assert!(
        unresolved.is_empty(),
        "test-only modules without a file: {unresolved:?}"
    );
    sources
        .iter()
        .filter(|(file, _)| !test_files.contains(*file))
        .map(|(file, text)| (file.clone(), library_code(text)))
        .collect()
}

fn mentions(line: &str, calls: &[&str]) -> bool {
    calls.iter().any(|call| line.contains(call))
}

/// True when `line` holds a `static` item or type bound, not a `'static`
/// lifetime.
fn has_static_item(line: &str) -> bool {
    line.match_indices("static ").any(|(at, _)| {
        let before = line[..at].chars().next_back();
        before != Some('\'') && !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// Every rule of this test applied to `sources`: the problems found, each
/// naming its file and function.
fn violations(sources: &BTreeMap<String, String>) -> BTreeSet<String> {
    let files = library_lines(sources);
    let bodies: BTreeMap<&String, _> = files
        .iter()
        .map(|(file, lines)| (file, fn_bodies(lines)))
        .collect();
    let mut found = BTreeSet::new();

    // Stale entries of the lists above.
    for (file, name) in DENY_FNS {
        let exists = bodies
            .get(&file.to_string())
            .is_some_and(|fns| fns.iter().any(|(_, n, _)| n == name));
        if !exists {
            found.insert(format!(
                "{file}: deny list names `{name}`, which is not there"
            ));
        }
    }
    for file in DENY_FILES.iter().chain(DISPATCH_FILES) {
        if !files.contains_key(*file) {
            found.insert(format!("{file}: listed in a rule, but not a library file"));
        }
    }
    // (8) every listed synchronous site names a function that exists.
    for (file, name, _) in HOT_SYNC_SITES {
        let exists = bodies
            .get(&file.to_string())
            .is_some_and(|fns| fns.iter().any(|(_, n, _)| n == name));
        if !exists {
            found.insert(format!(
                "{file}: the list of synchronous sites names `{name}`, which is not there"
            ));
        }
    }

    for (file, fns) in &bodies {
        let in_telemetry = file.starts_with("telemetry/");
        for (_, name, body) in fns {
            let dispatches = body.iter().any(|line| mentions(line, DISPATCH_CALLS));
            if !dispatches {
                continue;
            }
            // (1) only the listed files emit.
            if !in_telemetry && !DISPATCH_FILES.contains(&file.as_str()) {
                found.insert(format!(
                    "{file}: fn `{name}` calls dispatch, which this file may not"
                ));
            }
            // (2) never from a drop, the reaper or the sweeps, or the files
            // whose code runs under a lock or on an abort path.
            if name == "drop" {
                found.insert(format!("{file}: fn `drop` calls dispatch"));
            }
            if DENY_FNS.contains(&(file.as_str(), name.as_str())) {
                found.insert(format!("{file}: fn `{name}` calls dispatch"));
            }
            if DENY_FILES.contains(&file.as_str()) {
                found.insert(format!("{file}: fn `{name}` calls dispatch"));
            }
        }
    }
    for (file, lines) in &files {
        if DENY_FILES.contains(&file.as_str()) && lines.iter().any(|l| mentions(l, DISPATCH_CALLS))
        {
            found.insert(format!(
                "{file}: calls dispatch, which no code in this file may"
            ));
        }
    }

    // (6) the synchronous emits are made from the listed functions only, and
    // (7) never from a drop, a helper, the subtree, the stop lane or the
    // reaper and the sweeps.
    for (file, fns) in &bodies {
        for (_, name, body) in fns {
            if !body.iter().any(|line| mentions(line, HOT_EMIT_CALLS)) {
                continue;
            }
            let listed = HOT_SYNC_SITES
                .iter()
                .any(|(site_file, site_fn, _)| site_file == file && site_fn == name);
            if !listed {
                found.insert(format!(
                    "{file}: fn `{name}` runs handlers on its own task, which this function may not"
                ));
            }
            if name == "drop"
                || NO_SYNC_FILES.contains(&file.as_str())
                || DENY_FNS.contains(&(file.as_str(), name.as_str()))
            {
                found.insert(format!(
                    "{file}: fn `{name}` runs handlers on a path that owes the crate a step"
                ));
            }
        }
    }
    for (file, lines) in &files {
        if NO_SYNC_FILES.contains(&file.as_str())
            && lines.iter().any(|l| mentions(l, HOT_EMIT_CALLS))
        {
            found.insert(format!(
                "{file}: runs handlers on its own task, which no code in this file may"
            ));
        }
    }

    // (9) an event is queued with the plain push only from a listed function.
    for (file, name, _) in PLAIN_PUSH_SITES {
        let exists = bodies
            .get(&file.to_string())
            .is_some_and(|fns| fns.iter().any(|(_, n, _)| n == name));
        if !exists {
            found.insert(format!(
                "{file}: the list of plain pushes names `{name}`, which is not there"
            ));
        }
    }
    for (file, fns) in &bodies {
        for (_, name, body) in fns {
            if !body.iter().any(|line| mentions(line, PLAIN_PUSH_CALLS)) {
                continue;
            }
            let listed = PLAIN_PUSH_SITES
                .iter()
                .any(|(site_file, site_fn, _)| site_file == file && site_fn == name);
            if !listed {
                found.insert(format!(
                    "{file}: fn `{name}` queues an event with the plain push, which counts it in no actor's busy count"
                ));
            }
        }
    }

    // (3) the sinks are reached from their listed functions only.
    for (call, sites) in DELIVER_SITES {
        for (file, lines) in &files {
            let hits = lines.iter().filter(|l| l.contains(call)).count();
            if hits == 0 {
                continue;
            }
            let allowed: Vec<&str> = sites
                .iter()
                .filter(|(site_file, _)| site_file == file)
                .map(|(_, site_fn)| *site_fn)
                .collect();
            if allowed.is_empty() {
                found.insert(format!(
                    "{file}: `{call}` outside the files that may deliver"
                ));
                continue;
            }
            let inside = bodies[file]
                .iter()
                .filter(|(_, name, _)| allowed.contains(&name.as_str()))
                .flat_map(|(_, _, body)| body)
                .filter(|l| l.contains(call))
                .count();
            if inside != hits {
                found.insert(format!("{file}: `{call}` outside fn {allowed:?}"));
            }
        }
    }

    // (5) no state shared by the whole process in the telemetry module, and
    // no lookup of a system or an actor by id or name.
    for (file, lines) in &files {
        if !file.starts_with("telemetry/") {
            continue;
        }
        for line in lines {
            if line.contains("thread_local!") && !THREAD_LOCAL_FILES.contains(&file.as_str()) {
                found.insert(format!("{file}: thread_local state: `{line}`"));
            }
            // A per-thread flag declared inside `thread_local!` is a
            // `static` item to the text scan; the files that may keep one
            // are listed above.
            let thread_flag = THREAD_LOCAL_FILES.contains(&file.as_str()) && line.contains("Cell<");
            if has_static_item(line) && !thread_flag {
                found.insert(format!("{file}: static state: `{line}`"));
            }
            if mentions(line, LOOKUPS) {
                found.insert(format!("{file}: looks a system or actor up: `{line}`"));
            }
        }
    }

    // (6) every write to a stop lane queues its request's event.
    let mut seen_raise_fns = BTreeSet::new();
    for (file, fns) in &bodies {
        if file.as_str() == LANE_FILE {
            continue;
        }
        for (_, name, body) in fns {
            if !body.iter().any(|line| mentions(line, RAISE_CALLS)) {
                continue;
            }
            match RAISE_FNS
                .iter()
                .find(|(f, n, _)| *f == file.as_str() && *n == name.as_str())
            {
                None => {
                    found.insert(format!(
                        "{file}: fn `{name}` raises on a stop lane and queues no event \
                         (route it through raise_noted)"
                    ));
                }
                Some((f, n, how)) => {
                    seen_raise_fns.insert((*f, *n));
                    if *how == "emits" && !body.iter().any(|line| line.contains(REQUEST_EVENT)) {
                        found.insert(format!(
                            "{file}: fn `{name}` raises on a stop lane and does not queue its request event"
                        ));
                    }
                }
            }
        }
    }
    for (file, name, _) in RAISE_FNS {
        if !seen_raise_fns.contains(&(*file, *name)) {
            found.insert(format!(
                "{file}: raise list names `{name}`, which does not raise on a stop lane"
            ));
        }
    }

    // (7) a task of an incarnation is aborted only through the record, which
    // queues the abort's event ahead of the events of the task's end.
    let mut seen_abort_fns = BTreeSet::new();
    for (file, fns) in &bodies {
        for (_, name, body) in fns {
            if !body.iter().any(|line| mentions(line, ABORT_CALLS)) {
                continue;
            }
            if ABORT_FNS
                .iter()
                .any(|(f, n, _)| *f == file.as_str() && *n == name.as_str())
            {
                seen_abort_fns.insert((file.as_str(), name.as_str()));
            } else {
                found.insert(format!(
                    "{file}: fn `{name}` aborts a task itself \
                     (route it through AbortRecord::abort)"
                ));
            }
        }
    }
    for (file, name, _) in ABORT_FNS {
        if !seen_abort_fns.contains(&(*file, *name)) {
            found.insert(format!(
                "{file}: abort list names `{name}`, which does not abort a task"
            ));
        }
    }

    // The gate's own functions do no more than the load and the call.
    for file in HOT_FILES {
        let Some(fns) = bodies.get(&file.to_string()) else {
            found.insert(format!("{file}: not a library file"));
            continue;
        };
        for (owner, name, body) in fns {
            if !HOT_FNS.contains(&name.as_str()) {
                continue;
            }
            for line in body.iter().filter(|l| mentions(l, HOT_BANNED)) {
                found.insert(format!(
                    "{file}: gate fn `{name}` ({}) does more than load and call: `{line}`",
                    owner.as_deref().unwrap_or("no impl")
                ));
            }
        }
    }

    // The identity is recorded before the registry entry is visible.
    let runtime = "actor/runtime.rs";
    let spawn = bodies.get(&runtime.to_string()).and_then(|fns| {
        fns.iter()
            .find(|(_, name, _)| name == "spawn_actor_parts_in")
    });
    match spawn {
        None => {
            found.insert(format!("{runtime}: fn `spawn_actor_parts_in` is missing"));
        }
        Some((_, _, body)) => {
            let bind = body.iter().position(|l| l.contains("mode.bind("));
            let register = body.iter().position(|l| l.contains("register_actor"));
            match (bind, register) {
                (Some(b), Some(r)) if b < r => {}
                _ => {
                    found.insert(format!(
                        "{runtime}: fn `spawn_actor_parts_in` records the telemetry identity \
                         after it registers the actor, or not at all"
                    ));
                }
            }
        }
    }

    // (9) a log field that prints text derived from a failure goes through
    // a `LogFields` method that hides it for a sensitive actor, in every log
    // macro (`caught_log!` and `helper_log!`); the files that still print it
    // are counted in `USER_TEXT_UNREDACTED`.
    for (file, lines) in &files {
        let fields = unredacted_fields(lines);
        let allowed = USER_TEXT_UNREDACTED
            .iter()
            .find(|(f, _)| f == file)
            .map_or(0, |(_, n)| *n);
        if fields.len() > allowed {
            found.insert(format!(
                "{file}: a log macro (caught_log!, helper_log!) prints user text without a redacting call in {} fields ({} allowed): {fields:?}",
                fields.len(),
                allowed
            ));
        } else if fields.len() < allowed {
            found.insert(format!(
                "{file}: the list of unredacted log fields allows {allowed}, only {} are there",
                fields.len()
            ));
        }
    }
    for (file, _) in USER_TEXT_UNREDACTED {
        if !files.contains_key(*file) {
            found.insert(format!(
                "{file}: listed as holding log fields, but not a library file"
            ));
        }
    }

    // (9a) every library macro that writes a log line is a listed log macro,
    // and no library line writes a tracing event itself, so row (9) reads
    // every line that prints text and a new macro or a direct call cannot
    // print around it.
    for (file, lines) in &files {
        for (i, line) in lines.iter().enumerate() {
            if let Some(at) = line.find("macro_rules!") {
                let name = line[at + "macro_rules!".len()..]
                    .trim_start()
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or("");
                // The macro's definition: the lines to its closing brace.
                let mut depth = 0i32;
                let mut body = String::new();
                for later in &lines[i..] {
                    body.push_str(later);
                    body.push(' ');
                    depth += later.matches('{').count() as i32 - later.matches('}').count() as i32;
                    if depth <= 0 && body.contains('{') {
                        break;
                    }
                }
                let writes_a_line = body.contains("tracing::")
                    || LOG_MACROS
                        .iter()
                        .any(|log| body.contains(&format!("{log}!(")));
                if writes_a_line && !LOG_MACROS.contains(&name) {
                    found.insert(format!(
                        "{file}: macro `{name}` writes a log line and is not a listed log macro (list it in LOG_MACROS)"
                    ));
                }
            }
            for level in TRACING_EVENTS {
                let call = format!("{level}!(");
                let direct = line.match_indices(&call).any(|(at, _)| {
                    !line[..at]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_')
                });
                if direct {
                    found.insert(format!(
                        "{file}: calls the tracing macro `{level}!` itself (write the line through caught_log!): `{}`",
                        line.trim()
                    ));
                }
            }
        }
    }

    // (12) every helper spawn passes the owner its list entry names, and no
    // owner is built without the incarnation's record.
    let mut seen_spawns: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for (file, fns) in &bodies {
        for (_, name, body) in fns {
            for args in calls_of(body, HELPER_SPAWN_CALLS) {
                let is_spawn =
                    args.len() == 4 && (args[1].starts_with("HelperKind::") || args[1] == "kind");
                if !is_spawn {
                    continue;
                }
                let owner = args[2].as_str();
                let listed = HELPER_SPAWN_SITES
                    .iter()
                    .find(|(f, n, _, _)| *f == file.as_str() && *n == name.as_str());
                match listed {
                    None => {
                        found.insert(format!(
                            "{file}: fn `{name}` spawns a helper and is not on the list of helper spawns (owner `{owner}`)"
                        ));
                    }
                    Some((f, n, shape, _)) => {
                        *seen_spawns.entry((*f, *n)).or_default() += 1;
                        let right = match *shape {
                            "Some" => owner.starts_with("Some("),
                            "None" => owner == "None",
                            _ => owner == "owner",
                        };
                        if !right {
                            found.insert(format!(
                                "{file}: fn `{name}` spawns a helper with owner `{owner}`, the list says `{shape}`"
                            ));
                        }
                    }
                }
            }
            for args in calls_of(body, &["HelperOwner::new("]) {
                if args.get(1).map(String::as_str) == Some("None") {
                    found.insert(format!(
                        "{file}: fn `{name}` builds a helper owner with no record"
                    ));
                }
            }
        }
    }
    for (file, name, _, _) in HELPER_SPAWN_SITES {
        if !seen_spawns.contains_key(&(*file, *name)) {
            found.insert(format!(
                "{file}: the list of helper spawns names `{name}`, which spawns no helper"
            ));
        }
    }

    // (9b) a line about an actor's drop panic names the actor's sensitivity:
    // no library line builds fields with `unattributed()`, and the lines about
    // code that is no actor's go through `foreign()` in the listed files only.
    for (needle, allowed_in) in [
        ("LogFields::unattributed()", UNATTRIBUTED_CALLS),
        ("LogFields::foreign()", FOREIGN_TEXT_CALLS),
    ] {
        for (file, lines) in &files {
            let calls = lines.iter().filter(|line| line.contains(needle)).count();
            let allowed = allowed_in
                .iter()
                .find(|(f, _)| f == file)
                .map_or(0, |(_, n)| *n);
            if calls != allowed {
                found.insert(format!(
                    "{file}: {calls} calls of `{needle}` ({allowed} allowed)"
                ));
            }
        }
    }

    // (10) the code of the `sys` bundle, the monitor and the reports raises no
    // hot event and dispatches nothing: the one `Trace` raise sits inside the
    // listed `dispatch` function of the run loop.
    // (11) and it keeps no state of the process or the thread.
    for (file, lines) in &files {
        if !is_sys_file(file) {
            continue;
        }
        for line in lines {
            if mentions(line, HOT_EMIT_CALLS) || mentions(line, DISPATCH_CALLS) {
                found.insert(format!(
                    "{file}: emits or dispatches, which no code in this file may: `{line}`"
                ));
            }
            if line.contains("thread_local!") || has_static_item(line) {
                found.insert(format!(
                    "{file}: state of the process or the thread: `{line}`"
                ));
            }
        }
    }
    found
}

fn real_sources() -> BTreeMap<String, String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);
    assert!(sources.len() > 10, "the library sources were found");
    sources
}

#[test]
fn every_emit_site_of_the_library_goes_through_the_one_dispatch_function() {
    let found = violations(&real_sources());
    assert!(
        found.is_empty(),
        "{}",
        found.into_iter().collect::<Vec<_>>().join("\n")
    );
}

/// A source the real sources are changed by, and the problem the change
/// earns: the rule rows.
struct Row {
    name: &'static str,
    file: &'static str,
    /// Text of the file replaced, or `None` to append.
    find: Option<&'static str>,
    with: &'static str,
    /// Text the problem list must name.
    expect: &'static [&'static str],
}

const ROWS: &[Row] = &[
    Row {
        name: "dispatch from the reaper's fire",
        file: "system.rs",
        find: Some("fn fire(mut self) -> Option<Self> {"),
        with: "fn fire(mut self) -> Option<Self> { block.dispatch(kind, || event);",
        expect: &["system.rs", "fn `fire`"],
    },
    Row {
        name: "dispatch from the sweep",
        file: "system.rs",
        find: Some("async fn concurrent_force_sweep("),
        with: "async fn concurrent_force_sweep() { block.dispatch(kind, || event); }\nasync fn old_sweep(",
        expect: &["system.rs", "fn `concurrent_force_sweep`"],
    },
    Row {
        name: "dispatch from any drop",
        file: "actor/handle.rs",
        find: None,
        with: "impl Drop for Watcher { fn drop(&mut self) { self.block.dispatch(kind, || event); } }",
        expect: &["actor/handle.rs", "fn `drop`"],
    },
    Row {
        name: "dispatch from a helper's end guard",
        file: "helper.rs",
        find: None,
        with: "impl EndGuard { fn settle(&self) { self.block.dispatch(kind, || event); } }",
        expect: &["helper.rs", "fn `settle`"],
    },
    Row {
        name: "a stop raise with no event, in a supervisor's slot",
        file: "actor/supervision/slot.rs",
        find: None,
        with: "fn quiet(lane: &StopLane) { lane.try_raise(StopReason::Graceful); }",
        expect: &["actor/supervision/slot.rs", "fn `quiet`", "raises on a stop lane"],
    },
    Row {
        name: "a stop raise with no event, in the link guard",
        file: "actor/supervision.rs",
        find: None,
        with: "fn quiet(lane: &StopLane) { lane.raise(StopReason::Kill); }",
        expect: &["actor/supervision.rs", "fn `quiet`", "raises on a stop lane"],
    },
    Row {
        name: "the reaper's raise loses its request event",
        file: "system.rs",
        find: Some(
            "emit_stop_requested(\n                    None,\n                    &lane,\n                    &StopReason::Kill,\n                    result,\n                    crate::telemetry::StopOrigin::Requested,\n                    None,\n                );",
        ),
        with: "",
        expect: &["system.rs", "fn `fire`", "request event"],
    },
    Row {
        name: "an incarnation's task aborted directly, past the record",
        file: "actor/supervision/slot.rs",
        find: None,
        with: "fn quiet_abort(task: &tokio::task::AbortHandle) { task.abort(); }",
        expect: &["actor/supervision/slot.rs", "fn `quiet_abort`", "aborts a task itself"],
    },
    Row {
        name: "the record's abort loses its abort",
        file: "actor/subtree.rs",
        find: Some("        task.abort();\n    }\n\n    /// Records `now`"),
        with: "    }\n\n    /// Records `now`",
        expect: &["actor/subtree.rs", "abort list names `abort`"],
    },
    Row {
        name: "dispatch from the stop lane",
        file: "types/stop_lane.rs",
        find: None,
        with: "fn raise_again(block: &Block) { block.dispatch(kind, || event); }",
        expect: &["types/stop_lane.rs"],
    },
    Row {
        name: "dispatch from the run loop's end, before the init ack",
        file: "actor/runtime.rs",
        find: None,
        with: "fn end_inline(block: &Block) { block.dispatch(kind, || event); }",
        expect: &["actor/runtime.rs", "fn `end_inline`"],
    },
    Row {
        name: "dispatch from the verdict flag, before the cell is published",
        file: "actor/supervision/slot.rs",
        find: None,
        with: "fn report_inline(block: &Block) { block.dispatch(kind, || event); }",
        expect: &["actor/supervision/slot.rs", "fn `report_inline`"],
    },
    Row {
        name: "dispatch from a file not on the list",
        file: "actor/panic.rs",
        find: None,
        with: "fn contain(block: &Block) { block.dispatch(kind, || event); }",
        expect: &["actor/panic.rs", "fn `contain`"],
    },
    Row {
        name: "a handler called outside the sink function",
        file: "telemetry/sinks.rs",
        find: None,
        with: "fn shortcut(h: &[HandlerEntry]) { handlers::run(h, kind, &event); }",
        expect: &["telemetry/sinks.rs", "outside fn [\"deliver\"]"],
    },
    Row {
        name: "the sinks reached from another file",
        file: "actor/context.rs",
        find: None,
        with: "fn direct(mode: &Mode) { mode.sinks.deliver(mode, kind, event); }",
        expect: &["actor/context.rs", "sinks.deliver("],
    },
    Row {
        name: "a process-wide static in the telemetry module",
        file: "telemetry/mode.rs",
        find: None,
        with: "static HITS: AtomicU64 = AtomicU64::new(0);",
        expect: &["telemetry/mode.rs", "static state"],
    },
    Row {
        name: "a thread-local flag outside the handler file",
        file: "telemetry/identity.rs",
        find: None,
        with: "thread_local! { static BUSY: Cell<bool> = const { Cell::new(false) }; }",
        expect: &["telemetry/identity.rs", "thread_local state"],
    },
    Row {
        name: "the gate read through a lookup by name",
        file: "telemetry/mode.rs",
        find: Some("self.gate.word.load(Ordering::Relaxed) & kind.bit() != 0"),
        with: "ActorSystem::get_named(\"x\").unwrap().telemetry_mode().armed(kind)",
        expect: &["telemetry/mode.rs", "gate fn `armed`"],
    },
    Row {
        name: "a lookup anywhere in the telemetry module",
        file: "telemetry/sinks.rs",
        find: None,
        with: "fn scope_of(id: &ActorId) { let _ = systems().get_by_id(id); }",
        expect: &["telemetry/sinks.rs", "looks a system or actor up"],
    },
    Row {
        name: "an actor id cloned where the gate is read",
        file: "telemetry/mode.rs",
        find: Some("self.gate.word.load(Ordering::Relaxed) & kind.bit() != 0"),
        with: "{ let _id = ActorId::from(\"x\").clone(); self.gate.word.load(Ordering::Relaxed) & kind.bit() != 0 }",
        expect: &["telemetry/mode.rs", "gate fn `armed`"],
    },
    Row {
        name: "a cached emit from a function not on the list",
        file: "actor/context.rs",
        find: None,
        with: "fn shortcut(b: &Block) { b.emit_hot(cache, kind, || event); }",
        expect: &["actor/context.rs", "fn `shortcut`"],
    },
    Row {
        name: "a cached emit by path from a function not on the list",
        file: "actor/context.rs",
        find: None,
        with: "fn shortcut_path(r: &Record) { crate::telemetry::emit_hot(r, None, cache, kind, |_| event); }",
        expect: &["actor/context.rs", "fn `shortcut_path`"],
    },
    Row {
        name: "a locked emit by path from a function not on the list",
        file: "actor/handle.rs",
        find: None,
        with: "fn notify_path(r: &Record) { crate::telemetry::emit_slow(r, None, kind, |_| event); }",
        expect: &["actor/handle.rs", "fn `notify_path`"],
    },
    Row {
        name: "a locked emit from a function not on the list",
        file: "actor/handle.rs",
        find: None,
        with: "fn notify_inline(b: &Block) { b.emit_slow(kind, || event); }",
        expect: &["actor/handle.rs", "fn `notify_inline`"],
    },
    Row {
        name: "a cached emit from any drop",
        file: "actor/handle.rs",
        find: None,
        with: "impl Drop for Watcher { fn drop(&mut self) { self.block.emit_hot(cache, kind, || event); } }",
        expect: &["actor/handle.rs", "fn `drop`", "owes the crate a step"],
    },
    Row {
        name: "a listed function in a file where no emit may appear",
        file: "types/stop_lane.rs",
        find: None,
        with: "fn dispatch(b: &Block) { b.emit_slow(kind, || event); }",
        expect: &["types/stop_lane.rs", "fn `dispatch`", "owes the crate a step"],
    },
    Row {
        name: "a locked emit from a helper's end",
        file: "helper.rs",
        find: None,
        with: "impl EndGuard { fn settle(&self) { self.block.emit_slow(kind, || event); } }",
        expect: &["helper.rs", "fn `settle`", "owes the crate a step"],
    },
    Row {
        name: "a cached emit from the subtree",
        file: "actor/subtree.rs",
        find: None,
        with: "fn release_inline(b: &Block) { b.emit_hot(cache, kind, || event); }",
        expect: &["actor/subtree.rs", "fn `release_inline`"],
    },
    Row {
        name: "a locked emit from the reaper's fire",
        file: "system.rs",
        find: Some("fn fire(mut self) -> Option<Self> {"),
        with: "fn fire(mut self) -> Option<Self> { block.emit_slow(kind, || event);",
        expect: &["system.rs", "fn `fire`", "owes the crate a step"],
    },
    Row {
        name: "a listed site that no longer exists",
        file: "actor/runtime.rs",
        find: Some("async fn run_actor<A: Actor>("),
        with: "async fn run_actoq<A: Actor>(",
        expect: &["actor/runtime.rs", "`run_actor`, which is not there"],
    },
    Row {
        name: "a cached emit from the timer's next-tick function",
        file: "actor/context.rs",
        find: None,
        with: "fn adjust_next(b: &Block) { b.emit_hot(cache, kind, || event); }",
        expect: &["actor/context.rs", "fn `adjust_next`", "runs handlers on its own task"],
    },
    Row {
        name: "a cached emit from the mailbox's reserve poll",
        file: "actor/mailbox.rs",
        find: None,
        with: "fn poll(b: &Block) { crate::telemetry::emit_hot(r, None, cache, kind, |_| event); }",
        expect: &["actor/mailbox.rs", "fn `poll`", "runs handlers on its own task"],
    },
    Row {
        name: "a log field that prints a panic payload as it is",
        file: "actor/context.rs",
        find: None,
        with: "fn leak(payload: &str) { caught_log!(warn, actor_id = %id, panic = %payload, \"x\"); }",
        expect: &["actor/context.rs", "caught_log!", "\"panic\""],
    },
    Row {
        name: "a log field built with fields that know no actor",
        file: "actor/runtime.rs",
        find: None,
        with: "fn leak(payload: &str) { caught_log!(warn, panic = %LogFields::unattributed().panic_text(payload), \"x\"); }",
        expect: &["actor/runtime.rs", "LogFields::unattributed()"],
    },
    Row {
        name: "a log field about a foreign panic outside the listed files",
        file: "actor/runtime.rs",
        find: None,
        with: "fn leak(payload: &str) { caught_log!(warn, panic = %LogFields::foreign().panic_text(payload), \"x\"); }",
        expect: &["actor/runtime.rs", "LogFields::foreign()"],
    },
    Row {
        name: "a log field that prints an error as it is, on one line",
        file: "actor/panic.rs",
        find: None,
        with: "fn leak(err: &str) { caught_log!(warn, error = ?err, \"x\"); }",
        expect: &["actor/panic.rs", "caught_log!", "\"error\""],
    },
    Row {
        name: "a hot emit in the sys bundle",
        file: "actor/sys_state.rs",
        find: None,
        with: "fn shortcut(b: &Block) { b.emit_hot(cache, kind, || event); }",
        expect: &["actor/sys_state.rs", "emits or dispatches"],
    },
    Row {
        name: "a locked emit in the reports",
        file: "telemetry/reports.rs",
        find: None,
        with: "fn shortcut(b: &Block) { b.emit_slow(kind, || event); }",
        expect: &["telemetry/reports.rs", "emits or dispatches"],
    },
    Row {
        name: "a dispatch in the monitor",
        file: "telemetry/monitor.rs",
        find: None,
        with: "fn leak(b: &Block) { b.dispatch(kind, || event); }",
        expect: &["telemetry/monitor.rs", "emits or dispatches"],
    },
    Row {
        name: "a thread-local in the sys loop",
        file: "actor/sys_loop.rs",
        find: None,
        with: "thread_local! { static BUSY: Cell<bool> = const { Cell::new(false) }; }",
        expect: &["actor/sys_loop.rs", "state of the process or the thread"],
    },
    Row {
        name: "a mutable static in the sys debug state",
        file: "actor/sys_debug.rs",
        find: None,
        with: "static mut HITS: u64 = 0;",
        expect: &["actor/sys_debug.rs", "state of the process or the thread"],
    },
    Row {
        name: "a supervisor's death event back on the plain push",
        file: "actor/runtime.rs",
        find: Some("crate::telemetry::push_for(mode, owner.as_ref(), EventKind::ChildDied, || {"),
        with: "mode.push(EventKind::ChildDied, || {",
        expect: &["actor/runtime.rs", "fn `emit_child_died`", "plain push"],
    },
    Row {
        name: "a verdict event back on the plain push",
        file: "actor/supervision/slot.rs",
        find: Some("crate::telemetry::push_for(mode, owner.as_ref(), EventKind::Unresponsive, || {"),
        with: "mode.push(EventKind::Unresponsive, || {",
        expect: &["actor/supervision/slot.rs", "fn `report_verdict`", "plain push"],
    },
    Row {
        name: "a plain push from a new site",
        file: "actor/handle.rs",
        find: None,
        with: "fn announce(mode: &Arc<Mode>) { mode.push(EventKind::StopRequested, || event); }",
        expect: &["actor/handle.rs", "fn `announce`", "plain push"],
    },
    Row {
        name: "a listed plain push that no longer exists",
        file: "helper.rs",
        find: Some("fn record(&mut self, end: HelperEnd) {"),
        with: "fn recorded_end(&mut self, end: HelperEnd) {",
        expect: &["helper.rs", "plain pushes names `record`"],
    },
    Row {
        name: "a helper's end line back on the raw payload",
        file: "helper.rs",
        find: Some("panic = %fields.panic_text(payload),"),
        with: "panic = %payload,",
        expect: &["helper.rs", "helper_log!", "\"panic\""],
    },
    Row {
        name: "a new macro that writes a log line",
        file: "actor/context.rs",
        find: None,
        with: "macro_rules! shout { ($p:expr) => { caught_log!(warn, panic = %$p, \"x\") }; }",
        expect: &["actor/context.rs", "macro `shout`"],
    },
    Row {
        name: "a tracing event written around the log macros",
        file: "actor/context.rs",
        find: None,
        with: "fn leak(p: &str) { tracing::warn!(panic = %p, \"x\"); }",
        expect: &["actor/context.rs", "tracing macro `warn!`"],
    },
    Row {
        name: "a helper spawned from a function that is not listed",
        file: "actor/mailbox.rs",
        find: None,
        with: "fn sneak(sys: &ActorSystem, rt: &Handle) { sys.spawn_helper(rt, HelperKind::LateDrain, None, |end| end.observe(async {})); }",
        expect: &["actor/mailbox.rs", "fn `sneak`", "not on the list of helper spawns"],
    },
    Row {
        name: "the late drain spawned with no owner",
        file: "actor/runtime.rs",
        find: Some("system.spawn_helper(&rt, HelperKind::LateDrain, Some(helper_owner), |end| {"),
        with: "system.spawn_helper(&rt, HelperKind::LateDrain, None, |end| {",
        expect: &["actor/runtime.rs", "fn `end_queue_late`", "the list says `Some`"],
    },
    Row {
        name: "the late drain's owner built with no record",
        file: "actor/runtime.rs",
        find: Some("HelperOwner::new(owner.id().clone(), record.cloned())"),
        with: "HelperOwner::new(owner.id().clone(), None)",
        expect: &["actor/runtime.rs", "fn `end_queue_late`", "owner with no record"],
    },
    Row {
        name: "the stop ladder's owner built with no record",
        file: "actor/handle.rs",
        find: Some("HelperOwner::new(id.clone(), Some(lane.abort_record().clone()))"),
        with: "HelperOwner::new(id.clone(), None)",
        expect: &["actor/handle.rs", "fn `spawn_ladder`", "owner with no record"],
    },
    Row {
        name: "the monitor's owner built with no record",
        file: "system.rs",
        find: Some("Some(monitor.stop_lane_ref().abort_record().clone()),"),
        with: "None,",
        expect: &["system.rs", "fn `system_monitor`", "owner with no record"],
    },
    Row {
        name: "a clock read where the gate dispatches",
        file: "telemetry/mode.rs",
        find: Some("let event = build();\n        self.sinks.deliver"),
        with: "let _at = Instant::now(); let event = build();\n        self.sinks.deliver",
        expect: &["telemetry/mode.rs", "gate fn `dispatch`"],
    },
];

/// Applies `row` to a copy of `sources`.
fn changed(sources: &BTreeMap<String, String>, row: &Row) -> BTreeMap<String, String> {
    let mut out = sources.clone();
    let text = out.get_mut(row.file).expect("the row names a library file");
    match row.find {
        Some(find) => {
            assert!(
                text.contains(find),
                "row `{}`: `{find}` is not in {}",
                row.name,
                row.file
            );
            *text = text.replacen(find, row.with, 1);
        }
        None => {
            text.push('\n');
            text.push_str(row.with);
            text.push('\n');
        }
    }
    out
}

#[test]
fn each_rule_names_the_file_and_function_it_finds() {
    let real = real_sources();
    assert!(violations(&real).is_empty(), "the real sources are clean");
    let mut failures = Vec::new();
    for row in ROWS {
        let found = violations(&changed(&real, row));
        let text = found.iter().cloned().collect::<Vec<_>>().join("\n");
        if found.is_empty() || !row.expect.iter().all(|part| text.contains(part)) {
            failures.push(format!(
                "row `{}` should report {:?}, found:\n{text}",
                row.name, row.expect
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn an_identity_recorded_after_the_registration_is_found() {
    let real = real_sources();
    let mut late = real.clone();
    let runtime = late.get_mut("actor/runtime.rs").unwrap();
    let bind = "mode.bind(\n            abort_record.key(),";
    assert!(runtime.contains(bind), "the spawn records the identity");
    *runtime = runtime
        .replacen(bind, "mode.release(\n            abort_record.key(),", 1)
        .replacen(
            "target_system.attach_abort(",
            "mode.bind(abort_record.key(), identity);\n    target_system.attach_abort(",
            1,
        );
    let found = violations(&late);
    assert!(
        found
            .iter()
            .any(|p| p.contains("spawn_actor_parts_in")
                && p.contains("records the telemetry identity")),
        "{found:?}"
    );
}

#[test]
fn a_push_to_the_pump_is_allowed_in_a_drop() {
    let real = real_sources();
    let mut changed = real.clone();
    changed.get_mut("actor/handle.rs").unwrap().push_str(
        "\nimpl Drop for Watcher { fn drop(&mut self) { self.block.push_owned(owner, kind, || event); } }\n",
    );
    assert!(violations(&changed).is_empty());
}

fn lines(source: &str) -> Vec<String> {
    library_code(source)
}

#[test]
fn the_function_reader_follows_braces_through_nesting_and_headers() {
    let source = r#"
use std::fmt;

pub struct Guard { a: u8 }

impl Drop for Guard {
    fn drop(&mut self) {
        let f = |x: u8| {
            if x > 1 { helper(x); }
        };
        f(self.a);
    }
}

impl<'a, T> Holder<'a, T>
where
    T: Clone,
{
    pub(crate) async fn run(&self) -> u8 {
        match self.a {
            0 => { zero() }
            _ => other(),
        }
    }
    #[inline]
    fn one() -> u8 { 1 }
}

trait Doing {
    fn done(&self) { finish(); }
}

fn outer() -> impl Fn(u8) -> u8 {
    fn inner() { deep(); }
    let s = Shape { w: 1, h: 2 };
    move |x| x + s.w
}

#[cfg(test)]
mod tests {
    fn hidden() { never(); }
}
"#;
    let found = fn_bodies(&lines(source));
    let names: Vec<(Option<&str>, &str)> = found
        .iter()
        .map(|(owner, name, _)| (owner.as_deref(), name.as_str()))
        .collect();
    assert_eq!(
        names,
        vec![
            (Some("impl Drop for Guard"), "drop"),
            (Some("impl<'a, T> Holder<'a, T> where T: Clone,"), "run"),
            (Some("impl<'a, T> Holder<'a, T> where T: Clone,"), "one"),
            (None, "done"),
            (None, "outer"),
            (None, "inner"),
        ]
    );
    let body = |name: &str| -> String {
        found
            .iter()
            .find(|(_, n, _)| n == name)
            .unwrap()
            .2
            .join("|")
    };
    // The closure and its nested block are part of the drop's body.
    assert!(body("drop").contains("helper(x);"), "{}", body("drop"));
    assert!(body("drop").contains("f(self.a);"));
    // A match arm's block stays in its function.
    assert!(body("run").contains("zero()"));
    assert!(!body("run").contains("1"));
    // A one-line function holds just its statement.
    assert_eq!(body("one").trim(), "1");
    // The nested function is in its parent's body and in an entry of its own.
    assert!(body("outer").contains("deep();"));
    assert_eq!(body("inner").trim(), "deep();");
    // A struct literal's braces do not end the function.
    assert!(body("outer").contains("move |x| x + s.w"));
    // Test-only code is not read.
    assert!(!body("outer").contains("never"));
}

#[test]
fn each_listed_site_may_emit() {
    let real = real_sources();
    for (file, name, _) in HOT_SYNC_SITES {
        let mut changed = real.clone();
        changed.get_mut(*file).unwrap().push_str(&format!(
            "\nfn {name}(b: &Block) {{ b.emit_hot(cache, kind, || event); b.emit_slow(kind, || event); }}\n"
        ));
        let found = violations(&changed);
        assert!(found.is_empty(), "{file} fn {name}: {found:?}");
    }
}

#[test]
fn a_log_field_that_goes_through_text_is_allowed() {
    let real = real_sources();
    let mut changed = real.clone();
    changed.get_mut("actor/context.rs").unwrap().push_str(
        "\nfn quiet(fields: &Fields, payload: &str) { caught_log!(warn, panic = %fields.text(payload), \"x\"); caught_log!(warn, panic = %fields.panic_text(payload), reason = %fields.reason_text(r), \"x\"); helper_log!(self, panic = %fields.panic_text(payload), \"x\"); }\n",
    );
    assert!(violations(&changed).is_empty());
}

/// The one file that may install a scoped subscriber for a unit test.
const SCOPED_SUBSCRIBER_HOME: &str = "tracing_isolation.rs";

/// The lines of the library sources, test modules included, that install a
/// subscriber for the calling thread with `with_default(` or `set_default(`
/// outside the one helper that does it with every callsite kept dynamic, each
/// naming its file and line.
fn scoped_subscriber_installs(sources: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (file, text) in sources {
        if file == SCOPED_SUBSCRIBER_HOME {
            continue;
        }
        for (at, line) in source_scan::code_lines(text, false).iter().enumerate() {
            if line.contains("with_default(") || line.contains("set_default(") {
                found.insert(format!(
                    "{file}:{}: installs a scoped subscriber outside {SCOPED_SUBSCRIBER_HOME}, \
                     use `crate::tracing_isolation::install`: `{}`",
                    at + 1,
                    line.trim()
                ));
            }
        }
    }
    found
}

#[test]
fn no_unit_test_installs_a_scoped_subscriber_except_through_the_isolation_helper() {
    let real = real_sources();
    assert!(
        real.contains_key(SCOPED_SUBSCRIBER_HOME),
        "the helper file exists"
    );
    let found = scoped_subscriber_installs(&real);
    assert!(
        found.is_empty(),
        "{}",
        found.into_iter().collect::<Vec<_>>().join("\n")
    );

    // A call added to a test module is found, in either form; the helper's own
    // file, a comment and a literal are not.
    for (file, call) in [
        (
            "telemetry/turn_tests.rs",
            "tracing::subscriber::with_default(s, || {});",
        ),
        (
            "actor/panic.rs",
            "let _g = tracing::subscriber::set_default(s);",
        ),
    ] {
        let mut planted = real.clone();
        planted
            .get_mut(file)
            .expect("the row names a library file")
            .push_str(&format!("\nfn planted() {{ {call} }}\n"));
        let found = scoped_subscriber_installs(&planted);
        assert!(
            found.len() == 1 && found.iter().all(|p| p.starts_with(&format!("{file}:"))),
            "{file}: {found:?}"
        );
    }
    let mut quiet = real.clone();
    quiet
        .get_mut(SCOPED_SUBSCRIBER_HOME)
        .unwrap()
        .push_str("\nfn more() { tracing::subscriber::set_default(s); }\n");
    quiet
        .get_mut("actor/panic.rs")
        .unwrap()
        .push_str("\n// with_default(s, f)\nconst S: &str = \"set_default(\";\n");
    assert!(scoped_subscriber_installs(&quiet).is_empty());
}
