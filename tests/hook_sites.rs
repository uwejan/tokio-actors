//! Every call of a user hook in the library is made inside the catch that
//! builds its future: `catch_hook(|| actor.hook(..))`. A hook written as a
//! plain `fn` runs code before it returns its future, and that code must run
//! under `catch_unwind` like the rest of the hook. A call anywhere else fails
//! this test until it goes through `catch_hook`.

use std::path::Path;

#[path = "common/source_scan.rs"]
mod source_scan;

use source_scan::{library_code, rust_sources, test_only_files};

/// The hooks of the `Actor` trait the run loop calls on the actor value.
const HOOKS: &[&str] = &[
    "pre_start",
    "on_started",
    "handle",
    "pre_stop",
    "on_stopped",
    "on_child_stopped",
];

/// How many calls the library makes: the seven sites of the run loop.
const SITES: usize = 7;

/// The library code of `source` with its whitespace removed, so a call that
/// the formatter split over lines reads the same as one on a line.
fn squeezed(source: &str) -> String {
    library_code(source)
        .join(" ")
        .split_whitespace()
        .collect::<String>()
}

/// Calls of a hook on `actor` in `code`: how many there are, and the ones
/// that are not directly inside `catch_hook(||`.
fn hook_calls(code: &str) -> (usize, Vec<String>) {
    let mut total = 0;
    let mut uncaught = Vec::new();
    for hook in HOOKS {
        let call = format!("actor.{hook}(");
        let mut from = 0;
        while let Some(at) = code[from..].find(&call) {
            let at = from + at;
            total += 1;
            if !code[..at].ends_with("catch_hook(||") {
                let end = (at + 40).min(code.len());
                uncaught.push(code[at..end].to_string());
            }
            from = at + call.len();
        }
    }
    (total, uncaught)
}

#[test]
fn every_hook_call_of_the_library_is_built_inside_the_catch() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);
    assert!(sources.len() > 10, "the library sources were found");
    let (test_files, unresolved) = test_only_files(&sources);
    assert!(
        unresolved.is_empty(),
        "test-only modules without a file: {unresolved:?}"
    );
    let mut total = 0;
    let mut problems = Vec::new();
    for (relative, source) in &sources {
        if test_files.contains(relative) {
            continue;
        }
        let (found, uncaught) = hook_calls(&squeezed(source));
        total += found;
        for call in uncaught {
            problems.push(format!(
                "{relative}: `{call}...` calls a hook outside `catch_hook(||`"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    assert_eq!(
        total, SITES,
        "the library calls {SITES} hooks, found {total}"
    );
}

#[test]
fn the_scan_finds_a_hook_call_outside_the_catch() {
    let source = r#"
async fn run<A: Actor>(actor: &mut A, ctx: &mut ActorContext<A>) {
    let a = catch_hook(|| actor.pre_start(ctx)).await;
    let b = catch_callback(actor.on_started(ctx)).await;
    let c = catch_hook(
        || actor.handle(msg, ctx),
    )
    .await;
}

#[cfg(test)]
fn only_in_tests(actor: &mut A) {
    actor.pre_stop(&reason, ctx);
}
"#;
    let (total, uncaught) = hook_calls(&squeezed(source));
    assert_eq!(total, 3);
    assert_eq!(uncaught.len(), 1, "{uncaught:?}");
    assert!(uncaught[0].starts_with("actor.on_started("), "{uncaught:?}");
}
