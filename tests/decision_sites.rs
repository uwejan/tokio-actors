//! The restart, budget, strategy and stop decisions live in the supervision
//! rules module, as functions of their inputs. The scan reads the library
//! code (comments cut, literals emptied, test-only items and files left out)
//! of every file but the rules module and fails on: a line that names a
//! restart type, a restart strategy or the clean-exit test and is not listed
//! below with its reason; a function that reads the clock and also writes the
//! Unresponsive verdict, raises `Kill` or aborts a task, calls none of the
//! step functions of the rules module named in `STEPS` (matched as whole
//! names), and is not listed below by name with its reason; a listed line or function
//! that is gone. A function that takes a step function performs the action
//! the step names at the instant it read, and decides nothing itself.

use std::{collections::BTreeSet, path::Path};

#[path = "common/source_scan.rs"]
mod source_scan;

use source_scan::{fn_bodies, library_code, rust_sources, test_only_files};

const RULES: &str = "actor/supervision/rules.rs";

/// The step functions of the rules module, each the decision procedure of a
/// ladder or a wait.
const STEPS: [&str; 6] = [
    "ladder_step",
    "attempt_step",
    "successor_wait_step",
    "system_ladder_step",
    "verdict_wait_step",
    "strict_step",
];

/// Whether `body` calls `name` itself: `name(` with no identifier character
/// before it, so `system_ladder_step(` is no call of `ladder_step`.
fn calls_whole(body: &str, name: &str) -> bool {
    let call = format!("{name}(");
    body.match_indices(&call).any(|(at, _)| {
        body[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
    })
}

/// One row per listed site, tab separated: `line`, the file, the line as the
/// scan reads it and the reason (definitions, configuration defaults, display
/// mappings, and facts read at a site this module does not own); or `fn`, the
/// file, a function that reads the clock and acts on a task with the instant
/// it read, and the reason.
const LISTED: &str = r#"
line	application.rs	if matches!(kind, Some(RestartType::Permanent | RestartType::Transient)) {	the application core's own rule for a failed start, a pure step function with no clock
line	application.rs	Some(RestartType::Permanent) => true,	the application core's own rule for a top's end, a pure step function with no clock
line	application.rs	Some(RestartType::Transient) => !matches!(reason, StopReason::Graceful),	the application core's own rule for a top's end, a pure step function with no clock
line	actor/context.rs	if restart_type == RestartType::Permanent {	configuration check: a significant Permanent child spec is refused before any child runs
line	actor/context.rs	let pool = matches!(sup.config.strategy, RestartStrategy::SimpleOneForOne);	the pool fact handed to rules::classify_manual, which makes the decision
line	actor/context.rs	restart_type: RestartType::Permanent,	child spec default
line	actor/runtime.rs	&& sup.strategy == RestartStrategy::SimpleOneForOne =>	configuration check: auto_shutdown under SimpleOneForOne is refused at spawn
line	actor/supervision.rs	restart_type: child.map_or(RestartType::Permanent, |child| child.spec.restart_type),	default fact for a key with no spec
line	actor/supervision.rs	strategy: RestartStrategy::OneForAll,	configuration constructor
line	actor/supervision.rs	strategy: RestartStrategy::OneForOne,	configuration default
line	actor/supervision.rs	strategy: RestartStrategy::RestForOne,	configuration constructor
line	actor/supervision.rs	strategy: RestartStrategy::SimpleOneForOne,	configuration constructor
line	telemetry/reports.rs	RestartType::Permanent => true,	report mapping: which ends OTP reports as child_terminated; no restart follows from it
line	telemetry/reports.rs	RestartType::Transient | RestartType::Temporary => !reason.is_normal(),	report mapping: which ends OTP reports as child_terminated; no restart follows from it
fn	actor/context.rs	exit_with	exit signal raise through raise_kill_declared, which declares its deadline
fn	actor/context.rs	stop	the actor's own stop request; the Kill path goes through the declaring raiser
fn	actor/handle.rs	hand_off_steps	hands a lost stop task's owed Kill and abort to the reaper
fn	actor/handle.rs	stop	the handle's stop entry; a Kill goes through the declaring raiser
fn	actor/handle.rs	stop_within	the bounded stop call: window, Kill and the reaper's abort
fn	actor/link.rs	raise_caused	a link partner's Kill with a cause, through the declaring raiser
fn	actor/start_bound.rs	cut_start	a start past its timeout: Kill on the lane, then the abort at once
fn	actor/incarnation/owed.rs	abort	IncarnationRef::abort, the one path of every task abort
fn	actor/supervision.rs	drop	ChildLinkGuard::drop, the shell of the guard plan in the rules module
fn	actor/supervision/slot.rs	owe_kill_grace	start timeout kill path: Kill with its grace owed an abort
fn	actor/supervision/slot.rs	raise_owing	a ladder raise that owes the abort it records
fn	system.rs	concurrent_force_sweep	shutdown sweep of the roots left past the walk's deadline
fn	system.rs	fire	the reaper entry's stages: Kill, then the abort
fn	system.rs	kill	kill by name through the declaring raiser
fn	system.rs	kill_root	an application's lost stop handed on: Kill on the root through the declaring raiser, whose reaper owns the abort
fn	system.rs	kill_by_id_taking	kill by id through the declaring raiser and force_stop
fn	system.rs	kill_registered	the registry Kill raise behind raise_kill_declared
fn	system.rs	raise_shared	the step cell's shared raise of a bounded stop's Kill
fn	system.rs	raise_kill_declared	declares the Kill deadline, raises, arms the reaper
fn	system.rs	raise_noted_engaged	a raise with its telemetry event; the Kill deadline is declared by its callers
fn	actor/incarnation/stop_lane.rs	try_raise	the lane's raise itself
fn	actor/incarnation/stop_lane.rs	try_raise_kill_caused	the lane's raise of a Kill with a cause
"#;

fn decision_line(line: &str) -> bool {
    ["Permanent", "Transient", "Temporary"]
        .iter()
        .any(|t| line.contains(&format!("RestartType::{t}")))
        || line.contains("RestartStrategy::")
        || line.contains(".is_normal()")
}

fn reads_clock(body: &str) -> bool {
    ["Instant::now", ".elapsed(", "sleep_until(", "timeout_at("]
        .iter()
        .any(|p| body.contains(p))
}

fn acts(body: &str) -> bool {
    let verdict = body.contains(".set_for(") || body.contains(".set_for_subtree(");
    let kill = body.contains("raise_kill")
        || body.contains("raise_shared")
        || body.contains("accept_kill")
        || body.contains("kill_registered")
        || (body.contains("raise") && body.contains("StopReason::Kill"));
    let abort = body.contains(".abort(") || body.contains("abort_by_id(");
    verdict || kill || abort
}

#[test]
fn decision_sites() {
    let sources = rust_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    let (test_only, _) = test_only_files(&sources);
    let shipped = sources
        .iter()
        .filter(|(path, _)| path.as_str() != RULES && !test_only.contains(*path));
    let (mut lines, mut functions) = (BTreeSet::new(), BTreeSet::new());
    let mut bodies: Vec<(String, String, String)> = Vec::new();
    for (path, text) in shipped {
        let code = library_code(text);
        for line in code.iter().filter(|l| decision_line(l)) {
            lines.insert((path.clone(), line.clone()));
        }
        bodies.extend(
            fn_bodies(&code)
                .into_iter()
                .map(|(_, name, body)| (path.clone(), name, body.join("\n"))),
        );
    }
    // A function reads the clock when its body does, or when it calls a
    // function of the library that does (a bounded wait reads it inside).
    let mut clock: BTreeSet<String> = BTreeSet::new();
    loop {
        let before = clock.len();
        for (_, name, body) in &bodies {
            let calls = |n: &String| n.len() > 6 && body.contains(&format!("{n}("));
            if reads_clock(body) || clock.iter().any(calls) {
                clock.insert(name.clone());
            }
        }
        if clock.len() == before {
            break;
        }
    }
    let rules: BTreeSet<String> = fn_bodies(&library_code(&sources[RULES]))
        .into_iter()
        .map(|(_, name, _)| name)
        .collect();
    let missing: Vec<&str> = STEPS.into_iter().filter(|s| !rules.contains(*s)).collect();
    assert!(
        missing.is_empty(),
        "step functions not in the rules module: {missing:?}"
    );
    for (path, name, body) in &bodies {
        let direct = reads_clock(body)
            || clock
                .iter()
                .any(|n| n.len() > 6 && body.contains(&format!("{n}(")));
        let shell = STEPS.iter().any(|s| calls_whole(body, s));
        if direct && acts(body) && !shell {
            functions.insert((path.clone(), name.clone()));
        }
    }
    let (mut listed_lines, mut listed_fns) = (BTreeSet::new(), BTreeSet::new());
    for row in LISTED.lines().filter(|r| !r.is_empty()) {
        let cols: Vec<&str> = row.split('\t').collect();
        assert!(cols.len() == 4 && !cols[3].is_empty(), "bad row: {row}");
        let key = (cols[1].to_string(), cols[2].to_string());
        match cols[0] {
            "line" => listed_lines.insert(key),
            "fn" => listed_fns.insert(key),
            _ => panic!("bad row: {row}"),
        };
    }
    let unlisted: Vec<String> = lines
        .difference(&listed_lines)
        .map(|(f, l)| format!("line\t{f}\t{l}\t"))
        .chain(
            functions
                .difference(&listed_fns)
                .map(|(f, n)| format!("fn\t{f}\t{n}\t")),
        )
        .collect();
    let gone: Vec<String> = listed_lines
        .difference(&lines)
        .map(|(f, l)| format!("{f}: {l}"))
        .chain(
            listed_fns
                .difference(&functions)
                .map(|(f, n)| format!("{f}: fn {n}")),
        )
        .collect();
    println!(
        "decision_sites: {} lines, {} functions, {} unlisted, {} gone",
        lines.len(),
        functions.len(),
        unlisted.len(),
        gone.len()
    );
    assert!(
        unlisted.is_empty(),
        "decision sites outside the rules module; move the decision into the rules \
         module, or list a definition, a default or a display mapping:\n{}",
        unlisted.join("\n")
    );
    assert!(
        gone.is_empty(),
        "listed sites that are gone:\n{}",
        gone.join("\n")
    );
}
