//! Every call of a user hook in the library is made inside the catch that
//! builds its future: `catch_hook(|| actor.hook(..))`. A hook written as a
//! plain `fn` runs code before it returns its future, and that code must run
//! under `catch_unwind` like the rest of the hook. A call anywhere else fails
//! this test until it goes through `catch_hook`. The form that also carries
//! the hook's caller token, `catch_hook_t(token, || actor.hook(..))`, is the
//! same catch.

use std::path::Path;

#[path = "common/source_scan.rs"]
mod source_scan;

use source_scan::{library_code, rust_sources, test_only_files};

/// The hooks of the `Actor` trait the run loop calls on the actor value.
const HOOKS: &[&str] = &[
    "pre_start",
    "on_started",
    "on_idle",
    "handle",
    "pre_stop",
    "on_stopped",
    "on_child_stopped",
];

/// How many calls the library makes: the nine sites of the run loop (the
/// plain turn of the loop and the general turn each call `handle` once, the
/// idle fire calls `on_idle`, and `on_child_stopped` is called for a death
/// in the loop and for a final delivery).
const SITES: usize = 9;

/// The library code of `source` with its whitespace removed, so a call that
/// the formatter split over lines reads the same as one on a line.
fn squeezed(source: &str) -> String {
    library_code(source)
        .join(" ")
        .split_whitespace()
        .collect::<String>()
}

/// Whether the text before a hook call ends in the opening of a catch: the
/// closure of `catch_hook(` or of `catch_hook_t(<token>,`.
fn inside_catch(before: &str) -> bool {
    if before.ends_with("catch_hook(||") {
        return true;
    }
    let Some(head) = before
        .strip_suffix(",||")
        .or_else(|| before.strip_suffix(",||{"))
    else {
        return false;
    };
    head.rfind("catch_hook_t(").is_some_and(|open| {
        let token = &head[open + "catch_hook_t(".len()..];
        !token.contains([';', '{', '}'])
    })
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
            if !inside_catch(&code[..at]) {
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

/// The receive timeout's message function is the user's code too: the one
/// call of it in the library is made inside `catch_sync`.
#[test]
fn the_receive_timeout_map_is_called_inside_the_catch() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);
    let (_, turn) = sources
        .iter()
        .find(|(relative, _)| relative.ends_with("actor/turn.rs"))
        .expect("the turn state is in the library");
    let code = squeezed(turn);
    let calls: Vec<_> = code.match_indices("map()").collect();
    assert_eq!(calls.len(), 1, "one call of the function: {calls:?}");
    assert!(
        code[..calls[0].0].ends_with("catch_sync(||"),
        "the call is built inside the catch"
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
    let d = catch_hook_t(clock.caller_token(turn), || actor.pre_stop(&r, ctx)).await;
    let e = catch_hook_t(token, || { actor.on_stopped(&r, ctx) }).await;
    let f = catch_hook_t(token, || other()); actor.on_child_stopped(&ev, ctx);
}

#[cfg(test)]
fn only_in_tests(actor: &mut A) {
    actor.pre_stop(&reason, ctx);
}
"#;
    let (total, uncaught) = hook_calls(&squeezed(source));
    assert_eq!(total, 6);
    assert_eq!(uncaught.len(), 2, "{uncaught:?}");
    assert!(uncaught[0].starts_with("actor.on_started("), "{uncaught:?}");
    assert!(
        uncaught[1].starts_with("actor.on_child_stopped("),
        "{uncaught:?}"
    );
}
