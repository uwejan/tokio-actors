//! Real-clock waits and proxy reads stay out of the tests. The scan reads all
//! test code (comments cut, string literals emptied): each file under `tests/`,
//! each `src/` file that exists only under `cfg(test)`, and the test-only items
//! of every other `src/` file. Library code is never counted. It counts per file: `sleep(`; a `timeout(` whose duration
//! is not a literal of 30 s or more; a clock read (`Instant::now()`,
//! `.elapsed()`); a proxy read (`mailbox_len(`, `mailbox_available(`,
//! `yield_now(`). A row of `tests/common/shell_tests.txt` (`path`, `pattern`,
//! `count`, `reason`, tab separated) allows sites; a file over its row fails and
//! prints the row to write. A stale row fails only with `TIME_SITES_STRICT` set.

use std::{collections::BTreeMap, fs, path::Path};

#[path = "common/source_scan.rs"]
mod source_scan;

use source_scan::{code_lines, library_code, rust_sources, test_only_files};

const PATTERNS: [&str; 4] = ["sleep", "timeout", "clock", "proxy"];

/// Whether `arg` is a `Duration` literal of 30 s or more.
fn long_literal(arg: &str) -> bool {
    // Underscores go with the white space, so `from_secs` reads `fromsecs` below.
    let arg = arg.replace(|c: char| c.is_whitespace() || c == '_', "");
    let Some((path, call)) = arg.rsplit_once("Duration::") else {
        return false;
    };
    let Some((unit, tail)) = call.split_once('(') else {
        return false;
    };
    let digits = tail.strip_suffix(')').unwrap_or("").trim_end_matches("u64");
    let n = digits.parse::<u64>().unwrap_or(0);
    path.chars().all(|c| c.is_alphanumeric() || c == ':')
        && ((unit == "fromsecs" && n >= 30) || (unit == "frommillis" && n >= 30_000))
}

/// The text of the first argument of a call, given the text after its `(`.
fn first_arg(rest: &str) -> &str {
    let mut depth = 0i32;
    for (i, c) in rest.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth == 0 => return &rest[..i],
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => return &rest[..i],
            _ => {}
        }
    }
    rest
}

fn sites(pattern: &str, code: &str) -> usize {
    let n = |needle: &str| code.matches(needle).count();
    match pattern {
        "sleep" => n("sleep("),
        "clock" => n("Instant::now()") + n(".elapsed()"),
        "proxy" => n("mailbox_len(") + n("mailbox_available(") + n("yield_now("),
        _ => code
            .match_indices("timeout(")
            .filter(|(i, _)| !code[..*i].ends_with(|c: char| c.is_alphanumeric() || c == '_'))
            .filter(|(i, _)| !long_literal(first_arg(&code[i + "timeout(".len()..])))
            .count(),
    }
}

type Key = (String, String);

/// The code of every test file, by path, with the code of its library part:
/// the whole text of a file under `tests/` or of a `src/` file that exists
/// only under `cfg(test)` (no library part), and for any other `src/` file the
/// whole text and its library code, whose difference is its test-only items.
fn test_code(root: &Path) -> BTreeMap<String, (String, String)> {
    let mut files = BTreeMap::new();
    for (path, text) in rust_sources(&root.join("tests")) {
        let code = code_lines(&text, false).join("\n");
        files.insert(format!("tests/{path}"), (code, String::new()));
    }
    let sources = rust_sources(&root.join("src"));
    let (test_only, unresolved) = test_only_files(&sources);
    assert!(
        unresolved.is_empty(),
        "test-only modules without a file: {unresolved:?}"
    );
    for (path, text) in &sources {
        let code = code_lines(text, false).join("\n");
        let library = if test_only.contains(path) {
            String::new()
        } else {
            library_code(text).join("\n")
        };
        files.insert(format!("src/{path}"), (code, library));
    }
    files
}

#[test]
fn time_sites() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = test_code(root);
    let table = fs::read_to_string(root.join("tests/common/shell_tests.txt")).unwrap();
    let mut allowed: BTreeMap<Key, (usize, String)> = BTreeMap::new();
    for line in table
        .replace("\r\n", "\n")
        .lines()
        .filter(|l| !l.is_empty())
    {
        let cols: Vec<&str> = line.split('\t').collect();
        assert!(
            cols.len() == 4 && PATTERNS.contains(&cols[1]),
            "bad row: {line}"
        );
        let count = cols[2].parse().expect(line);
        allowed.insert((cols[0].into(), cols[1].into()), (count, cols[3].into()));
    }
    let (mut over, mut found) = (Vec::new(), BTreeMap::<Key, usize>::new());
    for (path, (code, library)) in &files {
        for pattern in PATTERNS {
            let n = sites(pattern, code).saturating_sub(sites(pattern, library));
            let key = (path.clone(), pattern.to_string());
            if n > 0 {
                found.insert(key.clone(), n);
            }
            if n > 0 && allowed.get(&key).is_none_or(|(count, _)| n > *count) {
                over.push(format!("{path}\t{pattern}\t{n}\tunclassified"));
            }
        }
    }
    let stale: Vec<String> = allowed
        .iter()
        .map(|(key, (count, reason))| (key, count, reason, found.get(key).copied().unwrap_or(0)))
        .filter(|(_, count, _, n)| n < *count)
        .map(|(k, count, reason, n)| {
            format!("{}\t{}\tallows {count}, holds {n}\t{reason}", k.0, k.1)
        })
        .collect();

    for pattern in PATTERNS {
        let hits: Vec<usize> = found
            .iter()
            .filter(|(k, _)| k.1 == pattern)
            .map(|(_, n)| *n)
            .collect();
        println!(
            "time_sites: {pattern} {} in {} files",
            hits.iter().sum::<usize>(),
            hits.len()
        );
    }
    let unclassified = allowed
        .values()
        .filter(|(_, reason)| reason == "unclassified")
        .count();
    println!(
        "time_sites: {unclassified} of {} rows unclassified",
        allowed.len()
    );
    stale
        .iter()
        .for_each(|row| println!("time_sites: stale row {row}"));
    assert!(
        over.is_empty(),
        "sites without a row, write or raise:\n{}",
        over.join("\n")
    );
    let strict = std::env::var_os("TIME_SITES_STRICT").is_some();
    assert!(
        stale.is_empty() || !strict,
        "stale rows:\n{}",
        stale.join("\n")
    );
}
