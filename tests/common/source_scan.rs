//! A reader of the library sources for the guard tests that scan them: the
//! code of a file without its comments, its string and character literals
//! emptied, and without the items that exist only under `cfg(test)`, and the
//! set of files that exist only under `cfg(test)`, derived from the `mod`
//! declarations that bring them in.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

/// The output of the lexer: lines of code.
struct Lines(Vec<String>);

impl Lines {
    fn ch(&mut self, c: char) {
        self.0.last_mut().expect("a line is open").push(c);
    }

    fn text(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.0.push(String::new());
            } else {
                self.ch(c);
            }
        }
    }

    fn newline(&mut self) {
        self.0.push(String::new());
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The lines of `source` with comments cut (line comments, and block comments
/// to any depth). String, raw string and character literals are emptied to
/// `""` and `''` unless `keep_literals`, so a brace, a slash or a call named
/// inside a literal is never read as code. A lifetime is kept as written.
pub fn code_lines(source: &str, keep_literals: bool) -> Vec<String> {
    let chars: Vec<char> = source.chars().collect();
    let n = chars.len();
    let mut out = Lines(vec![String::new()]);
    let mut block = 0usize;
    let mut i = 0;
    while i < n {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '\n' {
            out.newline();
            i += 1;
            continue;
        }
        if block > 0 {
            if c == '*' && next == Some('/') {
                block -= 1;
                i += 2;
            } else if c == '/' && next == Some('*') {
                block += 1;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('/') {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            block = 1;
            i += 2;
            continue;
        }
        if c == '"' {
            out.ch('"');
            i += 1;
            while i < n && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < n {
                    if keep_literals {
                        out.text(&format!("\\{}", chars[i + 1]));
                    } else if chars[i + 1] == '\n' {
                        out.newline();
                    }
                    i += 2;
                    continue;
                }
                if chars[i] == '\n' {
                    out.newline();
                } else if keep_literals {
                    out.ch(chars[i]);
                }
                i += 1;
            }
            out.ch('"');
            i += 1;
            continue;
        }
        if c == 'r' {
            let before = if i == 0 { None } else { Some(chars[i - 1]) };
            let starts_a_word = match before {
                None => true,
                Some('b') => i < 2 || !is_ident(chars[i - 2]),
                Some(p) => !is_ident(p),
            };
            if starts_a_word {
                let mut j = i + 1;
                while j < n && chars[j] == '#' {
                    j += 1;
                }
                if j < n && chars[j] == '"' {
                    let hashes = j - i - 1;
                    let mut close = vec!['"'];
                    close.extend(std::iter::repeat_n('#', hashes));
                    out.ch('"');
                    let mut k = j + 1;
                    loop {
                        if k >= n || chars[k..].starts_with(&close) {
                            break;
                        }
                        if chars[k] == '\n' {
                            out.newline();
                        } else if keep_literals {
                            out.ch(chars[k]);
                        }
                        k += 1;
                    }
                    out.ch('"');
                    i = (k + close.len()).min(n);
                    continue;
                }
            }
        }
        if c == '\'' {
            let a = next;
            if a == Some('\\') {
                let mut j = i + 3;
                while j < n && chars[j] != '\'' && chars[j] != '\n' {
                    j += 1;
                }
                if j < n && chars[j] == '\'' {
                    if keep_literals {
                        out.text(&chars[i..=j].iter().collect::<String>());
                    } else {
                        out.text("''");
                    }
                    i = j + 1;
                    continue;
                }
            } else if a.is_some_and(|a| a != '\n') && chars.get(i + 2).copied() == Some('\'') {
                if keep_literals {
                    out.text(&chars[i..i + 3].iter().collect::<String>());
                } else {
                    out.text("''");
                }
                i += 3;
                continue;
            }
            out.ch('\'');
            i += 1;
            continue;
        }
        out.ch(c);
        i += 1;
    }
    out.0
}

/// The index just past the attribute at the start of `text`, when it closes
/// on this line.
fn attr_end(text: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in text.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits `list` at the commas that are not inside brackets.
fn split_top(list: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in list.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&list[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&list[start..]);
    parts
}

/// Whether `attr` marks its item as existing only in test builds: exactly
/// `#[cfg(test)]` or `#[cfg(doctest)]`, or `#[cfg(all(test, ..))]` or
/// `#[cfg(all(doctest, ..))]`. An `any(test, ..)` or a `not(test)` item is
/// library code in some build.
fn test_only_cfg(attr: &str) -> bool {
    let compact: String = attr.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(inner) = compact
        .strip_prefix("#[cfg(")
        .and_then(|rest| rest.strip_suffix(")]"))
    else {
        return false;
    };
    if inner == "test" || inner == "doctest" {
        return true;
    }
    match inner
        .strip_prefix("all(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        Some(list) => {
            let parts = split_top(list);
            parts.contains(&"test") || parts.contains(&"doctest")
        }
        None => false,
    }
}

/// How far a skipped item reaches: through its closing brace, or, when it
/// has no body, through its `;` or `,` or up to the brace that closes the
/// scope around it. Brackets, parentheses and generic arguments are counted
/// so that a comma inside them does not end the item.
#[derive(Default)]
struct Item {
    depth: i32,
    nest: i32,
    opened: bool,
}

impl Item {
    /// The index in `text` just past the end of the item, or `None` when the
    /// item goes on past this text.
    fn end(&mut self, text: &str) -> Option<usize> {
        let mut prev = ' ';
        for (i, c) in text.char_indices() {
            match c {
                '(' | '[' => self.nest += 1,
                ')' | ']' => self.nest -= 1,
                '<' if !self.opened => self.nest += 1,
                '>' if !self.opened && prev != '-' && prev != '=' => {
                    self.nest = (self.nest - 1).max(0);
                }
                '{' => {
                    self.depth += 1;
                    self.opened = true;
                }
                '}' => {
                    if !self.opened && self.depth <= 0 && self.nest <= 0 {
                        return Some(i);
                    }
                    self.depth -= 1;
                    if self.opened && self.depth <= 0 {
                        return Some(i + 1);
                    }
                }
                ';' | ',' if !self.opened && self.nest <= 0 && self.depth <= 0 => {
                    return Some(i + 1);
                }
                _ => {}
            }
            prev = c;
        }
        None
    }
}

/// The code of `source` that is not test-only: comments cut, literals
/// emptied, and every item under a test-only `cfg` attribute skipped, a
/// module or a function or a statement alike. Each line is trimmed, and a
/// line left empty is dropped.
pub fn library_code(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = false;
    let mut item: Option<Item> = None;
    for line in code_lines(source, false) {
        let mut text = line.trim();
        let mut kept = String::new();
        while !text.is_empty() {
            if let Some(state) = item.as_mut() {
                match state.end(text) {
                    None => text = "",
                    Some(end) => {
                        item = None;
                        text = text[end..].trim_start();
                    }
                }
                continue;
            }
            if text.starts_with("#[") {
                let Some(end) = attr_end(text) else {
                    kept.push_str(text);
                    text = "";
                    continue;
                };
                if test_only_cfg(&text[..end]) {
                    pending = true;
                }
                text = text[end..].trim_start();
                continue;
            }
            if pending {
                pending = false;
                item = Some(Item::default());
                continue;
            }
            kept.push_str(text);
            text = "";
        }
        if !kept.is_empty() {
            out.push(kept);
        }
    }
    out
}

/// One `mod name;` declaration: the name, the file a `#[path]` attribute
/// names, and whether the declaration is test-only.
pub struct ModDecl {
    pub name: String,
    pub path: Option<String>,
    pub test_only: bool,
}

fn path_attr(attr: &str) -> Option<String> {
    let compact: String = attr.chars().filter(|c| !c.is_whitespace()).collect();
    compact
        .strip_prefix("#[path=\"")
        .and_then(|rest| rest.strip_suffix("\"]"))
        .map(str::to_string)
}

fn mod_decl_name(text: &str) -> Option<String> {
    let mut rest = text.strip_prefix("pub").map_or(text, |after| {
        let after = after.trim_start();
        match after.strip_prefix('(') {
            Some(inner) => inner.split_once(')').map_or(after, |(_, tail)| tail),
            None => after,
        }
    });
    rest = rest.trim_start().strip_prefix("mod ")?.trim_start();
    let name: String = rest.chars().take_while(|c| is_ident(*c)).collect();
    let tail = rest[name.len()..].trim_start();
    (!name.is_empty() && tail.starts_with(';')).then_some(name)
}

/// The `mod name;` declarations of `source`, each with the attributes that
/// precede it.
pub fn module_decls(source: &str) -> Vec<ModDecl> {
    let mut out = Vec::new();
    let mut test_only = false;
    let mut path = None;
    for line in code_lines(source, true) {
        let mut text = line.trim();
        while text.starts_with("#[") {
            let Some(end) = attr_end(text) else { break };
            let attr = &text[..end];
            if test_only_cfg(attr) {
                test_only = true;
            }
            if let Some(file) = path_attr(attr) {
                path = Some(file);
            }
            text = text[end..].trim_start();
        }
        if text.is_empty() {
            continue;
        }
        if let Some(name) = mod_decl_name(text) {
            out.push(ModDecl {
                name,
                path: path.take(),
                test_only,
            });
        }
        test_only = false;
        path = None;
    }
    out
}

/// The files `decl`, declared in `file`, can be.
fn files_of(file: &str, decl: &ModDecl) -> Vec<String> {
    let (dir, name) = match file.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", file),
    };
    let join = |base: &str, rest: &str| {
        if base.is_empty() {
            rest.to_string()
        } else {
            format!("{base}/{rest}")
        }
    };
    if let Some(path) = &decl.path {
        return vec![join(dir, path)];
    }
    let stem = name.strip_suffix(".rs").unwrap_or(name);
    let sub = if matches!(stem, "mod" | "lib" | "main") {
        dir.to_string()
    } else {
        join(dir, stem)
    };
    vec![
        join(&sub, &format!("{}.rs", decl.name)),
        join(&sub, &format!("{}/mod.rs", decl.name)),
    ]
}

/// The files that exist only under `cfg(test)`: those a test-only
/// declaration brings in, and every file declared from one of them. The
/// second list names the test-only declarations that match no file.
pub fn test_only_files(sources: &BTreeMap<String, String>) -> (BTreeSet<String>, Vec<String>) {
    let mut set = BTreeSet::new();
    let mut unresolved = Vec::new();
    for (file, text) in sources {
        for decl in module_decls(text).into_iter().filter(|d| d.test_only) {
            match files_of(file, &decl)
                .into_iter()
                .find(|candidate| sources.contains_key(candidate))
            {
                Some(found) => {
                    set.insert(found);
                }
                None => unresolved.push(format!("{file}: mod {}", decl.name)),
            }
        }
    }
    loop {
        let before = set.len();
        for file in set.clone() {
            for decl in module_decls(&sources[&file]) {
                if let Some(found) = files_of(&file, &decl)
                    .into_iter()
                    .find(|candidate| sources.contains_key(candidate))
                {
                    set.insert(found);
                }
            }
        }
        if set.len() == before {
            break;
        }
    }
    (set, unresolved)
}

/// Every Rust file under `dir`, by its path relative to `dir`, with `/`
/// separators, and its text.
pub fn rust_sources(dir: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(relative, fs::read_to_string(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}
