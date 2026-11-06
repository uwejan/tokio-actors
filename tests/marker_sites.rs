//! The task marker is read with relaxed loads and written with relaxed
//! stores because every clone of it lives inside the one future the actor's
//! task runs, so each read happens on the polling thread or in that future's
//! drop glue. A second call that hands out a clone could move one past the
//! future, where those orders no longer hold. This test fails on any library
//! call to `task_marker()` beyond the one that builds the actor's task.

use std::collections::BTreeMap;
use std::path::Path;

#[path = "common/source_scan.rs"]
mod source_scan;

use source_scan::{library_code, rust_sources, test_only_files};

/// The call that hands out a clone of the marker.
const CALL: &str = ".task_marker()";

/// The one library call: the file and the line as written (trimmed).
const ALLOWED_FILE: &str = "actor/runtime.rs";
const ALLOWED_LINE: &str = "let marker = context.task_marker();";

/// The library lines that call `task_marker()`.
fn marker_lines(source: &str) -> Vec<String> {
    library_code(source)
        .into_iter()
        .filter(|line| line.contains(CALL))
        .collect()
}

#[test]
fn only_the_actor_task_builder_takes_a_clone_of_the_task_marker() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);
    assert!(sources.len() > 10, "the library sources were found");
    let (test_files, unresolved) = test_only_files(&sources);
    assert!(
        unresolved.is_empty(),
        "test-only modules without a file: {unresolved:?}"
    );

    let mut problems = Vec::new();
    let mut allowed_seen = 0usize;
    for (relative, source) in &sources {
        if test_files.contains(relative) {
            continue;
        }
        for line in marker_lines(source) {
            if relative == ALLOWED_FILE && line == ALLOWED_LINE {
                allowed_seen += 1;
            } else {
                problems.push(format!(
                    "{relative}: `{line}` hands out a clone of the task marker outside the actor's task"
                ));
            }
        }
    }
    if allowed_seen != 1 {
        problems.push(format!(
            "{ALLOWED_FILE}: `{ALLOWED_LINE}` appears {allowed_seen} times, expected 1"
        ));
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_scan_finds_a_marker_call_and_skips_test_only_code() {
    let source = r#"
fn spawn_task(context: &Ctx) {
    let marker = context.task_marker(); // the one call
}

/// Docs that mention .task_marker() are not code.
#[cfg(test)]
fn only_in_tests(context: &Ctx) {
    let marker = context.task_marker();
}

#[cfg(all(test, feature = "tracing"))]
fn only_in_tracing_tests(context: &Ctx) {
    let marker = context.task_marker();
}

fn another(context: &Ctx) {
    leak(context.task_marker());
}
"#;
    assert_eq!(
        marker_lines(source),
        vec![
            "let marker = context.task_marker();".to_string(),
            "leak(context.task_marker());".to_string(),
        ]
    );
}

#[test]
fn code_that_is_library_code_in_some_build_is_scanned() {
    let source = r#"
#[cfg(any(test, feature = "tracing"))]
fn with_a_feature(context: &Ctx) {
    leak(context.task_marker());
}

#[cfg(not(test))]
fn without_tests(context: &Ctx) {
    leak(context.task_marker());
}

#[cfg(feature = "test_hooks")]
fn behind_a_feature(context: &Ctx) {
    leak(context.task_marker());
}
"#;
    assert_eq!(
        marker_lines(source),
        vec!["leak(context.task_marker());".to_string(); 3]
    );
}

#[test]
fn a_slash_in_a_string_does_not_hide_a_call_and_a_block_comment_does() {
    let source = r##"
fn after_a_string(context: &Ctx) {
    let url = "a//b"; leak(context.task_marker());
}

fn in_a_comment(context: &Ctx) {
    /* leak(context.task_marker()); */
    /* a /* nested */ leak(context.task_marker()); */
}

fn in_a_literal(context: &Ctx) {
    let text = "context.task_marker()";
    let raw = r#"context.task_marker() }"#;
    let brace = '}';
}

#[cfg(test)]
fn test_only_with_a_brace_in_a_string() {
    let text = "}";
    let raw = r#"}"#;
    leak(context.task_marker());
}

fn after_all_of_them(context: &Ctx) {
    leak(context.task_marker());
}
"##;
    assert_eq!(
        marker_lines(source),
        vec![
            "let url = \"\"; leak(context.task_marker());".to_string(),
            "leak(context.task_marker());".to_string(),
        ]
    );
}

#[test]
fn test_only_files_come_from_their_declarations() {
    let mut sources: BTreeMap<String, String> = [
        ("lib.rs", "mod helper;\n#[cfg(test)]\nmod probe_tests;\n#[cfg(not(test))]\nmod real;\n"),
        ("helper.rs", ""),
        ("probe_tests.rs", "mod deeper;\n"),
        ("probe_tests/deeper.rs", ""),
        ("real.rs", ""),
        (
            "actor/mod.rs",
            "#[cfg(test)]\n#[path = \"checks.rs\"]\nmod checks_inside;\nmod unit;\n#[cfg(test)]\npub(crate) mod harness;\n",
        ),
        ("actor/checks.rs", ""),
        ("actor/unit.rs", "#[cfg(test)]\nmod tests;\n"),
        ("actor/unit/tests.rs", ""),
        ("actor/harness/mod.rs", ""),
    ]
    .into_iter()
    .map(|(file, text)| (file.to_string(), text.to_string()))
    .collect();
    let (files, unresolved) = test_only_files(&sources);
    assert_eq!(
        files.iter().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "actor/checks.rs",
            "actor/harness/mod.rs",
            "actor/unit/tests.rs",
            "probe_tests.rs",
            "probe_tests/deeper.rs",
        ]
    );
    assert!(unresolved.is_empty(), "{unresolved:?}");

    sources.insert(
        "actor/missing.rs".to_string(),
        "#[cfg(test)]\nmod absent;\n".to_string(),
    );
    let (_, unresolved) = test_only_files(&sources);
    assert_eq!(unresolved, vec!["actor/missing.rs: mod absent".to_string()]);
}
