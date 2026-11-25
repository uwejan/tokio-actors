//! The source files sit in five layers, and a file imports its own layer or
//! a lower one: layer 0 holds the public plain types, layer 1 the pure cores
//! of the supervision rules, layer 2 the kernel (what an actor's life runs
//! without a call from outside), layer 3 the calls that only a user starts,
//! layer 4 the behaviours. The test reads each file's `crate::..` paths
//! outside test code and fails on an import that goes from a lower layer to a
//! higher one, or on a file that is in no layer.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[path = "common/source_scan.rs"]
mod source_scan;

use source_scan::{library_code, rust_files, test_only_files};

/// Path prefixes (relative to `src/`) and the layer each belongs to.
const LAYERS: &[(&str, u8)] = &[
    ("types/", 0),
    ("error.rs", 0),
    ("actor/supervision/model.rs", 1),
    ("actor/supervision/rules.rs", 1),
    ("system/actors.rs", 3),
    ("system/lifecycle.rs", 3),
    ("system/shutdown.rs", 3),
    ("system/monitoring.rs", 3),
    ("actor/context/subscriptions.rs", 3),
    ("actor/context/monitors.rs", 3),
    ("actor/context/links.rs", 3),
    ("actor/context/idle.rs", 3),
    ("actor/context/aliases.rs", 3),
    ("actor/context/requests.rs", 3),
    ("actor/handle/observe.rs", 3),
    ("actor/handle/send.rs", 3),
    ("actor/handle/stop.rs", 3),
    ("actor/spawn.rs", 3),
    ("actor/alias/send.rs", 3),
    ("actor/request/forward.rs", 3),
    ("telemetry/monitor/forward.rs", 3),
    ("application.rs", 4),
    ("behavior.rs", 4),
    ("event.rs", 4),
    ("otp_map.rs", 4),
    ("signals.rs", 4),
    ("statem.rs", 4),
    ("actor/", 2),
    ("helper", 2),
    ("lib.rs", 2),
    ("system", 2),
    ("telemetry/", 2),
    ("time_probe.rs", 2),
    ("tracing_isolation.rs", 2),
];

/// Upward edges that stay, with the reason each stays.
const KNOWN_EDGES: &[(&str, &str, &str)] = &[(
    "types/mod.rs",
    "actor/envelope.rs",
    "`types::Envelope` and `types::SignalTag` are public paths; the envelope is defined with the channels it travels on",
)];

fn layer_of(file: &str) -> Option<u8> {
    LAYERS
        .iter()
        .find(|(prefix, _)| file.starts_with(prefix))
        .map(|(_, layer)| *layer)
}

/// The module path of a source file: `actor/mod.rs` is `actor`.
fn module_of(file: &str) -> Vec<String> {
    let mut parts: Vec<String> = file
        .trim_end_matches(".rs")
        .split('/')
        .map(String::from)
        .collect();
    if matches!(parts.last().map(String::as_str), Some("mod" | "lib")) {
        parts.pop();
    }
    parts
}

/// Every `crate::` path in `code`, a brace group read as one path per item.
fn crate_paths(code: &str) -> Vec<Vec<String>> {
    let (mut out, mut from) = (Vec::new(), 0);
    while let Some(i) = code[from..].find("crate::") {
        let start = from + i + "crate::".len();
        let glued = code[..from + i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':');
        from = start;
        if glued {
            continue;
        }
        let (mut stack, mut prefix, mut cur, mut word) =
            (Vec::new(), Vec::<String>::new(), Vec::new(), String::new());
        for ch in code[start..].chars().chain(" ".chars()) {
            if ch.is_alphanumeric() || ch == '_' {
                word.push(ch);
                continue;
            }
            if !word.is_empty() {
                cur.push(std::mem::take(&mut word));
            }
            let emit = |out: &mut Vec<Vec<String>>, prefix: &[String], cur: &mut Vec<String>| {
                if !cur.is_empty() {
                    out.push([prefix, cur.as_slice()].concat());
                }
                cur.clear();
            };
            match ch {
                ':' => {}
                '{' => {
                    stack.push(prefix.clone());
                    prefix.append(&mut cur);
                }
                ',' if !stack.is_empty() => emit(&mut out, &prefix, &mut cur),
                '}' if !stack.is_empty() => {
                    emit(&mut out, &prefix, &mut cur);
                    prefix = stack.pop().unwrap_or_default();
                }
                c if c.is_whitespace() && !stack.is_empty() => {}
                _ => {
                    emit(&mut out, &prefix, &mut cur);
                    break;
                }
            }
        }
    }
    out
}

#[test]
fn layers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_files(&root);
    let (test_only, unresolved) = test_only_files(&sources);
    assert!(
        unresolved.is_empty(),
        "test-only modules without a file: {unresolved:?}"
    );
    let files: BTreeMap<String, Vec<String>> = sources
        .keys()
        .filter(|file| !test_only.contains(*file))
        .map(|file| (file.clone(), module_of(file)))
        .collect();
    let mut problems = Vec::new();
    let mut seen_known = BTreeSet::new();
    for (file, text) in sources.iter().filter(|(f, _)| files.contains_key(*f)) {
        let Some(from) = layer_of(file) else {
            problems.push(format!("{file} is in no layer"));
            continue;
        };
        let code = library_code(text).join("\n");
        let mut targets = BTreeSet::new();
        for path in crate_paths(&code) {
            let found = (1..=path.len())
                .rev()
                .find_map(|n| files.iter().find(|(_, module)| **module == path[..n]));
            if let Some((target, _)) = found {
                if target != file {
                    targets.insert(target.clone());
                }
            }
        }
        for target in targets {
            let Some(to) = layer_of(&target) else {
                continue;
            };
            if to <= from {
                continue;
            }
            match KNOWN_EDGES
                .iter()
                .find(|(f, t, _)| f == file && *t == target)
            {
                Some(_) => {
                    seen_known.insert((file.clone(), target));
                }
                None => problems.push(format!(
                    "{file} (layer {from}) imports {target} (layer {to})"
                )),
            }
        }
    }
    for (file, target, why) in KNOWN_EDGES {
        if !seen_known.contains(&(file.to_string(), target.to_string())) {
            problems.push(format!(
                "known edge {file} -> {target} no longer exists ({why})"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
