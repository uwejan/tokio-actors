//! A callback that is still awaiting is cancelled only by the abort that backs
//! a `Kill`, by the abort of a start that outruns its `start_timeout` (or whose
//! bound panics before it decides), or by the end of the actor's runtime. The
//! library issues an abort against an actor's task at a short list of sites,
//! each of which backs one of the first two; a new site fails this test until
//! it is classified.

#[path = "common/source_scan.rs"]
mod source_scan;

/// The files that call `.abort(` in production code, with the number of calls
/// each holds and what each one backs. Every call goes through the abort
/// record of the incarnation, except the record's own `task.abort()`.
const ABORT_SITES: &[(&str, usize, &str)] = &[
    (
        "actor/handle.rs",
        1,
        "the bounded stop's last step: the abort after the kill window",
    ),
    (
        "actor/start_bound.rs",
        1,
        "the start bound's cut, after the Kill it raises on the lane: at the deadline, and when its bound panics before it decides",
    ),
    (
        "actor/subtree.rs",
        1,
        "the abort record: the one `task.abort()` every other site goes through",
    ),
    (
        "actor/supervision/slot.rs",
        2,
        "the supervisor's stop ladder: the abort of a child after a Kill's grace",
    ),
    (
        "system.rs",
        3,
        "the abort by id after a Kill's grace, the forced stop's abort, and the reaper entry's one abort, which its fire and its drop both go through after a Kill",
    ),
];

/// The files that abort a whole set of forwarder tasks (timers and streams),
/// which run no callback of the actor's.
const FORWARDER_ABORT_FILES: &[&str] = &["actor/context.rs"];

#[test]
fn an_actor_task_is_aborted_only_at_the_classified_sites() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = source_scan::rust_sources(&root);
    let (test_only, unresolved) = source_scan::test_only_files(&sources);
    assert!(
        unresolved.is_empty(),
        "test-only modules without a file: {unresolved:?}"
    );

    let mut found: Vec<(String, usize)> = Vec::new();
    let mut forwarder_files: Vec<String> = Vec::new();
    let mut bare_aborts: Vec<String> = Vec::new();
    for (path, src) in &sources {
        if test_only.contains(path) {
            continue;
        }
        let text = source_scan::library_code(src).join("\n");
        let calls = text.matches(".abort(").count();
        if calls > 0 {
            found.push((path.clone(), calls));
        }
        if text.matches(".abort()").count() > 0 {
            bare_aborts.push(path.clone());
        }
        if text.contains("abort_all(") {
            forwarder_files.push(path.clone());
        }
    }

    let expected: Vec<(String, usize)> = ABORT_SITES
        .iter()
        .map(|(path, count, _)| ((*path).to_string(), *count))
        .collect();
    assert_eq!(
        found, expected,
        "an abort was added or removed: a callback is cancelled only by the abort that \
         backs a Kill or a start bound, so a new site needs a place in the list above"
    );
    assert_eq!(
        bare_aborts,
        vec!["actor/subtree.rs".to_string()],
        "a task is aborted directly outside the abort record"
    );
    assert_eq!(
        forwarder_files,
        FORWARDER_ABORT_FILES
            .iter()
            .map(|path| (*path).to_string())
            .collect::<Vec<_>>(),
        "a set of tasks is aborted at a site that is not classified"
    );
}

#[test]
fn the_scan_counts_abort_calls_in_shipped_code_only() {
    let src = r##"
fn shipped() { record.abort(&a); }

#[cfg(test)]
mod tests {
    fn nested() { record.abort(&b); task.abort(); }
}

fn also_shipped() { record.abort(&c); } // d.abort(&e)
"##;
    let text = source_scan::library_code(src).join("\n");
    assert_eq!(text.matches(".abort(").count(), 2, "{text}");
}
