//! A system's drop removes its own slot from the process map of named
//! systems, which takes the shard lock of the map. A thread that held a
//! shard's guard and then dropped the last reference to a system would take
//! that lock a second time and wait for itself. So no reference to a system
//! is made while the map's guard is held unless it leaves the function as its
//! return value, and a weak reference is upgraded only after the guard has
//! ended.
//!
//! What this test checks: it reads the library sources, without comments and
//! without test-only code, and fails on any `.upgrade()` line outside the
//! list below, which names for each the reason no map guard is held there,
//! and on any line that reaches the map and upgrades in one expression. The
//! scan reads text, so it cannot tell which type an `upgrade` is called on;
//! the reason beside each entry says.

use std::collections::BTreeMap;
use std::path::Path;

#[path = "common/source_scan.rs"]
mod source_scan;

use source_scan::{library_code, rust_sources, test_only_files};

struct Allowed {
    file: &'static str,
    line: &'static str,
    times: usize,
    why: &'static str,
}

const ALLOWED: &[Allowed] = &[
    Allowed {
        file: "system.rs",
        line: "weak.upgrade()",
        times: 1,
        why: "the lookup by name, after the block that holds the map's guard has ended",
    },
    Allowed {
        file: "system.rs",
        line: "let mut me = self.me.upgrade();",
        times: 1,
        why: "reactivate, before it takes the gate or the map's guard; the slot takes the reference or the caller drops it after both",
    },
    Allowed {
        file: "system.rs",
        line: "let Some(live) = system.upgrade() else {",
        times: 1,
        why: "the sweep, a task that holds no guard of the map",
    },
    Allowed {
        file: "system.rs",
        line: "if let Some(live) = system.upgrade() {",
        times: 1,
        why: "the sweep task's read after it ended, with no guard of the map held",
    },
    Allowed {
        file: "telemetry/mode.rs",
        line: "self.system.get().and_then(|weak| weak.0.upgrade())",
        times: 1,
        why: "the mode's weak system, read by the pump's start and its drain and by a handle's way to its system, none of which is reached from a code path that holds a guard of the process map",
    },
    Allowed {
        file: "telemetry/hot.rs",
        line: "match self.mode.upgrade() {",
        times: 1,
        why: "a weak telemetry mode, not a system: a loop's cache hands its handler list to the pump of the mode it was read from",
    },
    Allowed {
        file: "actor/context.rs",
        line: "let Some(factory) = factory.upgrade() else {",
        times: 1,
        why: "a weak factory of a timer, not a system",
    },
    Allowed {
        file: "actor/subtree.rs",
        line: ".and_then(|linked| linked.token.upgrade());",
        times: 1,
        why: "a subtree token, not a system",
    },
    Allowed {
        file: "actor/subtree.rs",
        line: "let token = self.shared.linked.get()?.token.upgrade()?;",
        times: 1,
        why: "a subtree token, not a system",
    },
    Allowed {
        file: "actor/subtree.rs",
        line: "if let (Some(spec), Some(token)) = (linked.trap.as_deref(), linked.token.upgrade())",
        times: 1,
        why: "the trap budget of a worker reads its own subtree token, a token and not a system, under no guard of the map",
    },
];

/// The library lines that upgrade a weak reference.
fn upgrade_lines(source: &str) -> Vec<String> {
    library_code(source)
        .into_iter()
        .filter(|line| line.contains(".upgrade("))
        .collect()
}

/// The library lines that reach the map and upgrade in the same line.
fn map_and_upgrade_lines(source: &str) -> Vec<String> {
    upgrade_lines(source)
        .into_iter()
        .filter(|line| line.contains("systems()"))
        .collect()
}

#[test]
fn no_library_line_upgrades_a_system_while_the_process_map_is_held() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);
    assert!(sources.len() > 10, "the library sources were found");
    let (test_files, unresolved) = test_only_files(&sources);
    assert!(
        unresolved.is_empty(),
        "test-only modules without a file: {unresolved:?}"
    );

    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut problems = Vec::new();
    for (relative, source) in &sources {
        if test_files.contains(relative) {
            continue;
        }
        for line in upgrade_lines(source) {
            *seen.entry((relative.clone(), line)).or_default() += 1;
        }
        for line in map_and_upgrade_lines(source) {
            problems.push(format!(
                "{relative}: `{line}` upgrades inside the map's guard"
            ));
        }
    }
    for ((file, line), times) in &seen {
        match ALLOWED
            .iter()
            .find(|allowed| allowed.file == file && allowed.line == line)
        {
            Some(allowed) if allowed.times == *times => {}
            Some(allowed) => problems.push(format!(
                "{file}: `{line}` appears {times} times, expected {} ({})",
                allowed.times, allowed.why
            )),
            None => problems.push(format!("{file}: `{line}` upgrades and is not listed")),
        }
    }
    for allowed in ALLOWED {
        if !seen.contains_key(&(allowed.file.to_string(), allowed.line.to_string())) {
            problems.push(format!(
                "{}: `{}` is listed and no longer appears",
                allowed.file, allowed.line
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_scan_finds_upgrades_and_skips_test_only_code() {
    let source = r#"
fn lookup(name: &str) -> Option<Arc<System>> {
    let found = systems().get(name)?.weak.upgrade();
    weak.upgrade()
}

/// Docs that mention .upgrade() are not code.
#[cfg(test)]
fn only_in_tests(weak: &Weak<System>) {
    weak.upgrade();
}

fn a_literal() -> &'static str {
    ".upgrade()"
}
"#;
    assert_eq!(
        upgrade_lines(source),
        vec![
            "let found = systems().get(name)?.weak.upgrade();".to_string(),
            "weak.upgrade()".to_string(),
        ]
    );
    assert_eq!(
        map_and_upgrade_lines(source),
        vec!["let found = systems().get(name)?.weak.upgrade();".to_string()]
    );
}
