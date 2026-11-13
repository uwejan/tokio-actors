//! Every task the crate spawns has an owner that observes its end. A task
//! spawned through the system's helper slot is recorded there; the other
//! spawn sites below each put the task in a `JoinSet` its owner joins, or are
//! the actor's own task, which its handle and the registry observe. A spawn
//! call anywhere else in the library fails this test until it goes through
//! the helper slot or is listed here with its owner.

use std::path::Path;

#[path = "common/source_scan.rs"]
mod source_scan;

use source_scan::{library_code, rust_sources, test_only_files};

/// One allowed spawn call: the file, the call's line as written (trimmed),
/// how many times that line appears in the file, and who observes the task.
struct Site {
    file: &'static str,
    line: &'static str,
    count: usize,
    owner: &'static str,
}

const ALLOWED: &[Site] = &[
    Site {
        file: "helper.rs",
        line: "rt.spawn(make(self.guard(rt, kind, owner)))",
        count: 1,
        owner: "the helper slot itself: it records each task's end once",
    },
    Site {
        file: "time_probe.rs",
        line: "let mut join = pin!(tokio::task::unconstrained(rt.spawn(async {})));",
        count: 1,
        owner: "the probe itself: the task does nothing, its handle is polled once and dropped, and a runtime that has ended cancels it",
    },
    Site {
        file: "actor/runtime.rs",
        line: "let join = span.spawn(&handle, run);",
        count: 1,
        owner: "the actor's own task, joined by its watcher and observed by the registry",
    },
    Site {
        file: "telemetry/span.rs",
        line: "return rt.spawn(fut);",
        count: 1,
        owner: "the actor's own task (no span), joined by its watcher and observed by the registry",
    },
    Site {
        file: "telemetry/span.rs",
        line: "rt.spawn(SpanRun {",
        count: 1,
        owner: "the actor's own task (inside its span), joined by its watcher and observed by the registry",
    },
    Site {
        file: "telemetry/span.rs",
        line: "rt.spawn(fut)",
        count: 1,
        owner: "the actor's own task (build without spans), joined by its watcher and observed by the registry",
    },
    Site {
        file: "telemetry/span.rs",
        line: "None => rt.spawn(fut),",
        count: 1,
        owner: "the actor's own task (no span), joined by its watcher and observed by the registry",
    },
    Site {
        file: "actor/runtime.rs",
        line: "death_set.spawn(async move {",
        count: 1,
        owner: "a supervisor's death plane, joined by its run loop",
    },
    Site {
        file: "actor/supervision.rs",
        line: "let task = self.restart_set.spawn(fut);",
        count: 1,
        owner: "a supervisor's restart plane, joined by its run loop",
    },
    Site {
        file: "actor/supervision.rs",
        line: "let task = self.death_set.spawn(async move {",
        count: 3,
        owner: "a supervisor's death plane, joined by its run loop",
    },
    Site {
        file: "actor/context.rs",
        line: "let abort = extras.forwarders.spawn_on(forwarder, &runtime);",
        count: 1,
        owner: "an actor's forwarder set, joined by its run loop",
    },
    Site {
        file: "system.rs",
        line: "drop(self.helpers.spawn(rt, HelperKind::Sweep, None, |end| {",
        count: 1,
        owner: "the helper slot",
    },
    Site {
        file: "system.rs",
        line: "self.helpers.spawn(rt, kind, owner, make)",
        count: 1,
        owner: "the helper slot",
    },
    Site {
        file: "system.rs",
        line: ".spawn(rt, HelperKind::Reaper, None, |end| {",
        count: 1,
        owner: "the helper slot",
    },
    Site {
        file: "system.rs",
        line: "let task = waits.spawn(async move {",
        count: 1,
        owner: "the shutdown walk's wait set, joined by the walk",
    },
    Site {
        file: "system.rs",
        line: "set.spawn(async move { force_stop(rx, None, record, grace, requested).await });",
        count: 1,
        owner: "the sweep's force-stop set, joined by the sweep",
    },
    Site {
        file: "system.rs",
        line: "set.spawn(async move { force_stop(rx, abort, record, grace, requested).await });",
        count: 1,
        owner: "the sweep's force-stop set, joined by the sweep",
    },
];

/// The spellings of a call that starts a task or a thread.
const SPAWN_CALLS: &[&str] = &[
    ".spawn(",
    ".spawn_on(",
    ".spawn_local(",
    ".spawn_blocking(",
    "tokio::spawn(",
    "task::spawn(",
    "task::spawn_blocking(",
    "task::spawn_local(",
    "thread::spawn(",
];

/// The library lines that call a spawn function.
fn spawn_lines(source: &str) -> Vec<String> {
    library_code(source)
        .into_iter()
        .filter(|line| SPAWN_CALLS.iter().any(|call| line.contains(call)))
        .collect()
}

#[test]
fn every_spawn_site_of_the_library_is_listed_with_its_owner() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);
    assert!(sources.len() > 10, "the library sources were found");
    let (test_files, unresolved) = test_only_files(&sources);
    assert!(
        unresolved.is_empty(),
        "test-only modules without a file: {unresolved:?}"
    );

    let mut problems = Vec::new();
    let mut seen = vec![0usize; ALLOWED.len()];
    for (relative, source) in &sources {
        if test_files.contains(relative) {
            continue;
        }
        for line in spawn_lines(source) {
            match ALLOWED
                .iter()
                .position(|site| site.file == relative.as_str() && site.line == line)
            {
                Some(at) => seen[at] += 1,
                None => problems.push(format!(
                    "{relative}: `{line}` spawns a task outside the helper slot and the listed sites"
                )),
            }
        }
    }
    for (site, found) in ALLOWED.iter().zip(&seen) {
        if *found != site.count {
            problems.push(format!(
                "{}: `{}` appears {found} times, the list expects {} ({})",
                site.file, site.line, site.count, site.owner
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_scan_finds_a_spawn_and_skips_test_only_code() {
    let source = r#"
fn helper_free() {
    let task = tokio::spawn(async {}); // a spawn outside the slot
}

/// Docs that mention .spawn( are not code.
#[cfg(test)]
fn only_in_tests() {
    tokio::spawn(async {});
}

#[cfg(test)]
mod tests {
    fn nested() {
        std::thread::spawn(|| {});
    }
}

fn another() {
    rt.spawn(work());
}
"#;
    let found = spawn_lines(source);
    assert_eq!(
        found,
        vec![
            "let task = tokio::spawn(async {});".to_string(),
            "rt.spawn(work());".to_string(),
        ]
    );
}

#[test]
fn code_that_is_library_code_in_some_build_is_scanned() {
    let source = r##"
#[cfg(any(test, feature = "tracing"))]
fn with_a_feature() {
    tokio::spawn(async {});
}

fn after_a_string() {
    let url = "a//b"; rt.spawn(work());
    /* rt.spawn(hidden()); */
}

#[cfg(all(test, feature = "tracing"))]
fn only_in_tracing_tests() {
    let text = "}";
    rt.spawn(work());
}

fn last() {
    rt.spawn_blocking(work());
}
"##;
    assert_eq!(
        spawn_lines(source),
        vec![
            "tokio::spawn(async {});".to_string(),
            "let url = \"\"; rt.spawn(work());".to_string(),
            "rt.spawn_blocking(work());".to_string(),
        ]
    );
}
