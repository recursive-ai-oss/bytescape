//! The helper never starts another program. This reads the source of the
//! helper and of every crate of this repository that it links, on Linux and on
//! Windows, and fails on any name that starts a program or a copy of the
//! process: the `Command` type, the Windows process and shell calls, the C
//! library's `exec*`, `fork`, `clone` and `posix_spawn*` families, and the
//! system call numbers that `libc::syscall` would take for them.
//!
//! Everything is read except comments, strings and the items marked as
//! compiled for tests only (wherever in the file they are), and the files that
//! hold only tests. The standard library contains such code; nothing the
//! helper links may call it. A system call number passed as a plain integer
//! cannot be seen by a name search; the only `libc::syscall` calls in the
//! linked crates are listed in docs/security/helper.md for review.
//!
//! The lint configuration of the helper (clippy.toml) forbids the same things
//! at compile time, for the helper's own code. This test also covers the
//! crates it depends on.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Words that start a program (matched as whole words, outside comments,
/// strings and test code). The `SYS_` names are the system call numbers that
/// `libc::syscall` takes: calling one of them directly starts a program or a
/// copy of this one just as well as the wrappers do.
const FORBIDDEN: &[&str] = &[
    "Command",
    "CommandExt",
    "CreateProcessA",
    "CreateProcessW",
    "CreateProcessAsUserW",
    "CreateProcessWithLogonW",
    "CreateProcessWithTokenW",
    "ShellExecuteA",
    "ShellExecuteW",
    "ShellExecuteExA",
    "ShellExecuteExW",
    "WinExec",
    "execv",
    "execve",
    "execvp",
    "execvpe",
    "execl",
    "execle",
    "execlp",
    "fexecve",
    "posix_spawn",
    "posix_spawnp",
    "SYS_execve",
    "SYS_execveat",
    "SYS_fork",
    "SYS_vfork",
    "SYS_clone",
    "SYS_clone3",
];

/// Names that are ordinary words in Rust code, and count only when they are
/// reached through the C library (`libc::name`, or inside `use libc::{..}`).
const FORBIDDEN_IN_LIBC: &[&str] = &["system", "popen", "fork", "vfork", "clone", "clone3"];

/// Name beginnings that count in the same two places: the whole `exec*` and
/// `posix_spawn*` families, whatever their exact name.
const FORBIDDEN_LIBC_PREFIXES: &[&str] = &["exec", "posix_spawn"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

/// The crates of this repository in the helper's dependency closure for
/// `target`, found with `cargo tree`.
fn closure_dirs(target: Option<&str>) -> BTreeSet<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = std::process::Command::new(cargo);
    command.current_dir(workspace_root()).args([
        "tree",
        "-p",
        "bytescape-helper",
        "-e",
        "normal",
        "--prefix",
        "none",
        "--offline",
        "--locked",
    ]);
    if let Some(target) = target {
        command.args(["--target", target]);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let mut dirs = BTreeSet::new();
    for line in text.lines() {
        // "name v0.1.0 (/path/to/crate)" for crates of this repository.
        if let Some(open) = line.find(" (/").or_else(|| line.find(" (")) {
            let rest = &line[open + 2..];
            let end = rest.find(')').unwrap_or(rest.len());
            let path = PathBuf::from(&rest[..end]);
            if path.is_dir() {
                dirs.insert(path);
            }
        }
    }
    dirs
}

/// The text of a Rust file with comments, strings and character literals
/// replaced by blanks. Lines, braces and everything else are kept, so what is
/// left can be searched for names and its braces matched. The one string that
/// is kept is the file name in a `#[path = "…"]` attribute: it says where a
/// module's file is.
fn blank_non_code(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        let after_word = i > 0 && is_word(c[i - 1]);
        match ch {
            '/' if next == Some('/') => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                // Block comments nest.
                let mut depth = 0usize;
                while i < c.len() {
                    if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                        depth = depth.saturating_sub(1);
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if c[i] == '\n' {
                            out.push('\n');
                        }
                        i += 1;
                    }
                }
                out.push(' ');
            }
            // Raw strings, with the byte (`b`) and C (`c`) prefixes.
            'r' | 'b' | 'c' if !after_word && raw_string_end(&c, i).is_some() => {
                let end = raw_string_end(&c, i).unwrap_or(c.len());
                out.extend(c[i..end].iter().filter(|x| **x == '\n'));
                out.push(' ');
                i = end;
            }
            '"' => {
                let start = i;
                let keep = follows_path_attribute(&out);
                i += 1;
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                let end = i.min(c.len());
                if keep {
                    out.extend(&c[start..end]);
                } else {
                    out.extend(c[start..end].iter().filter(|x| **x == '\n'));
                    out.push(' ');
                }
            }
            '\'' => {
                // A character literal, or the start of a lifetime.
                if next == Some('\\') {
                    i += 2;
                    while i < c.len() && c[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                    out.push(' ');
                } else if next.is_some() && c.get(i + 2) == Some(&'\'') {
                    i += 3;
                    out.push(' ');
                } else {
                    out.push('\'');
                    i += 1;
                }
            }
            _ => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
}

/// Whether the text so far ends in `#[path =`, so that the string that comes
/// next names a module's file.
fn follows_path_attribute(so_far: &str) -> bool {
    so_far
        .trim_end()
        .strip_suffix('=')
        .and_then(|rest| rest.trim_end().strip_suffix("path"))
        .is_some_and(|rest| rest.trim_end().ends_with('['))
}

/// Where the raw string that starts at `start` (`r"…"`, `r#"…"#`, `br"…"`,
/// `cr"…"`) ends, if one starts there.
fn raw_string_end(c: &[char], start: usize) -> Option<usize> {
    let mut i = start;
    if matches!(c.get(i), Some('b' | 'c')) {
        i += 1;
    }
    if c.get(i) != Some(&'r') {
        return None;
    }
    i += 1;
    let mut hashes = 0usize;
    while c.get(i) == Some(&'#') {
        hashes += 1;
        i += 1;
    }
    if c.get(i) != Some(&'"') {
        return None;
    }
    i += 1;
    while i < c.len() {
        if c[i] == '"' && (0..hashes).all(|k| c.get(i + 1 + k) == Some(&'#')) {
            return Some(i + 1 + hashes);
        }
        i += 1;
    }
    Some(c.len())
}

/// The arguments of a list, split at the commas that are not inside brackets.
fn split_top_level(list: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut from = 0;
    for (at, ch) in list.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&list[from..at]);
                from = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&list[from..]);
    parts
}

/// Whether a `cfg` condition holds only when compiling tests: `test`, an
/// `all(…)` with `test` among its terms, or an `any(…)` whose terms all are.
/// `not(test)` and `any(test, unix)` can hold in a normal build, so what they
/// guard is production code.
fn only_in_tests(condition: &str) -> bool {
    let list = |prefix: &str| {
        condition
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(')'))
            .map(split_top_level)
    };
    condition == "test"
        || list("all(").is_some_and(|terms| terms.into_iter().any(only_in_tests))
        || list("any(").is_some_and(|terms| terms.into_iter().all(only_in_tests))
}

/// The index after the whitespace that starts at `i`.
fn skip_space(c: &[char], mut i: usize) -> usize {
    while c.get(i).is_some_and(|ch| ch.is_whitespace()) {
        i += 1;
    }
    i
}

/// The index after the group that opens at `open` (`[`, `(` or `{`), or the
/// end of the text if it never closes.
fn group_end(c: &[char], open: usize) -> usize {
    let mut depth = 0i32;
    let mut i = open;
    while i < c.len() {
        match c[i] {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    c.len()
}

fn word_at(c: &[char], i: usize) -> String {
    c.get(i..)
        .unwrap_or_default()
        .iter()
        .take_while(|ch| ch.is_alphanumeric() || **ch == '_')
        .collect()
}

/// Words that begin an item (or a `let`), whose end is its `;` or its braces
/// and not the next comma: `fn f<A, B>()` has commas of its own.
const ITEM_WORDS: &[&str] = &[
    "fn",
    "mod",
    "struct",
    "enum",
    "trait",
    "impl",
    "use",
    "const",
    "static",
    "type",
    "extern",
    "unsafe",
    "async",
    "let",
    "macro_rules",
];

/// Whether what starts at `from` (after a `#[cfg(test)]`) is an item or a
/// statement, and not a field, a variant, a match arm, a parameter or a
/// field of a struct literal. Those end at a comma.
fn starts_item(c: &[char], from: usize) -> bool {
    let mut i = skip_space(c, from);
    // Further attributes, then the visibility.
    while c.get(i) == Some(&'#') {
        let open = skip_space(c, i + 1 + usize::from(c.get(i + 1) == Some(&'!')));
        if c.get(open) != Some(&'[') {
            break;
        }
        i = skip_space(c, group_end(c, open));
    }
    let mut word = word_at(c, i);
    if word == "pub" {
        i = skip_space(c, i + 3);
        if c.get(i) == Some(&'(') {
            i = skip_space(c, group_end(c, i));
        }
        word = word_at(c, i);
    }
    match word.as_str() {
        // A contextual keyword: `union: u8` is a field.
        "union" => c.get(skip_space(c, i + word.len())) != Some(&':'),
        w => ITEM_WORDS.contains(&w),
    }
}

/// Where the thing an attribute applies to ends, when it starts at `from`:
///
/// - an item or statement, after its `;` or its braces;
/// - a field, variant, match arm, parameter or literal field, after its comma;
/// - and in every case before the bracket or brace that closes the list or
///   scope it is in, which is left for the caller.
fn item_end(c: &[char], from: usize) -> usize {
    let item = starts_item(c, from);
    let mut depth = 0i32;
    let mut i = from;
    while i < c.len() {
        match c[i] {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            ';' if depth == 0 => return i + 1,
            ',' if depth == 0 && !item => return i + 1,
            '{' if depth == 0 => return group_end(c, i),
            '}' if depth == 0 => return i,
            _ => {}
        }
        i += 1;
    }
    c.len()
}

/// Where the scope that contains `from` ends: its closing brace, which is
/// left for the caller, or the end of the text.
fn scope_end(c: &[char], from: usize) -> usize {
    let mut depth = 0i32;
    for (at, ch) in c.iter().enumerate().skip(from) {
        match ch {
            '{' => depth += 1,
            '}' => {
                if depth == 0 {
                    return at;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    c.len()
}

/// `code` without the things that exist only in test builds, and the text of
/// each thing taken out. A `#[cfg(test)]` item may sit anywhere in a file, so
/// only what the attribute applies to is dropped (a `mod tests;` line is just
/// that line, a field is just that field) and the rest is kept.
///
/// An inner `#![cfg(test)]` makes its whole scope test code: the file when it
/// is at the top of the file, otherwise the inline module it is in.
fn strip_test_items(code: &str, dropped: &mut Vec<String>) -> String {
    let c: Vec<char> = code.chars().collect();
    let mut out = String::with_capacity(code.len());
    let mut braces = 0usize;
    let mut i = 0;
    while i < c.len() {
        if c[i] == '#' {
            let inner = c.get(i + 1) == Some(&'!');
            let open = skip_space(&c, i + 1 + usize::from(inner));
            if c.get(open) == Some(&'[') {
                let close = group_end(&c, open).saturating_sub(1);
                let attribute: String = c[(open + 1).min(c.len())..close.min(c.len())]
                    .iter()
                    .filter(|ch| !ch.is_whitespace())
                    .collect();
                let condition = attribute
                    .strip_prefix("cfg(")
                    .and_then(|rest| rest.strip_suffix(')'));
                if condition.is_some_and(only_in_tests) {
                    let end = match (inner, braces) {
                        (false, _) => item_end(&c, close + 1),
                        (true, 0) => c.len(),
                        (true, _) => scope_end(&c, close + 1),
                    };
                    dropped.push(c[i..end.min(c.len())].iter().collect());
                    out.push(' ');
                    i = end;
                    continue;
                }
            }
        }
        match c[i] {
            '{' => braces += 1,
            '}' => braces = braces.saturating_sub(1),
            _ => {}
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

fn without_test_items(code: &str) -> String {
    strip_test_items(code, &mut Vec::new())
}

/// The code of a file outside comments, strings and test-only items.
fn code_of(text: &str) -> String {
    without_test_items(&blank_non_code(text))
}

/// Code with every run of white space made one space, and none left around
/// `::` or before `{`, so that `libc :: fork` and `extern "C" {` have one form.
fn squeeze(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut space = false;
    for ch in code.chars() {
        if ch.is_whitespace() {
            space = true;
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(ch);
        }
    }
    out.replace(" ::", "::")
        .replace(":: ", "::")
        .replace(" {", "{")
}

/// What follows each `libc::` in `squeezed`: the name, or the whole group of
/// `libc::{…}`.
fn libc_paths(squeezed: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    let mut rest = squeezed;
    while let Some(at) = rest.find("libc::") {
        rest = &rest[at + "libc::".len()..];
        if rest.starts_with('{') {
            let chars: Vec<char> = rest.chars().collect();
            let end: usize = chars[..group_end(&chars, 0)]
                .iter()
                .map(|ch| ch.len_utf8())
                .sum();
            paths.push(&rest[..end]);
        } else {
            let end = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .unwrap_or(rest.len());
            // A glob is `*`, which ends the name before it starts.
            paths.push(if end == 0 {
                &rest[..rest.len().min(1)]
            } else {
                &rest[..end]
            });
        }
    }
    paths
}

/// The names declared inside `extern { … }` blocks of `squeezed`.
fn extern_names(squeezed: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = squeezed;
    while let Some(at) = rest.find("extern{") {
        rest = &rest[at + "extern{".len()..];
        let block = &rest[..rest.find('}').unwrap_or(rest.len())];
        names.extend(
            block
                .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .filter(|w| !w.is_empty()),
        );
    }
    names
}

fn forbidden_through_libc(name: &str) -> bool {
    FORBIDDEN_IN_LIBC.contains(&name)
        || FORBIDDEN.contains(&name)
        || FORBIDDEN_LIBC_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The forbidden names that `code` uses, each once.
///
/// A `libc::*` glob and a `libc as …` alias are reported too: they hide the
/// names this looks for behind a shorter path, and the closure has no need of
/// either.
fn hits(code: &str) -> Vec<String> {
    let mut found: BTreeSet<String> = words(code)
        .filter(|w| FORBIDDEN.contains(w))
        .map(str::to_owned)
        .collect();
    let squeezed = squeeze(code);
    for path in libc_paths(&squeezed) {
        if path.contains('*') {
            found.insert(String::from("libc::*"));
        }
        let group: Vec<&str> = words(path).collect();
        if group.windows(2).any(|pair| pair == ["self", "as"]) {
            found.insert(String::from("libc as"));
        }
        for name in group {
            if forbidden_through_libc(name) {
                found.insert(name.to_owned());
            }
        }
    }
    if words(&squeezed)
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| pair == ["libc", "as"])
    {
        found.insert(String::from("libc as"));
    }
    // In functions declared by hand.
    for name in extern_names(&squeezed) {
        if forbidden_through_libc(name) {
            found.insert(name.to_owned());
        }
    }
    found.into_iter().collect()
}

fn words(code: &str) -> impl Iterator<Item = &str> {
    code.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// A `mod name;` line: the module's file is elsewhere.
struct ModDecl {
    name: String,
    /// The file named by a `#[path = "…"]` attribute, if there is one.
    path: Option<String>,
}

/// The file-backed modules that `code` (comments and strings blanked) declares
/// at the top level of the file. One inside an inline module is somewhere this
/// does not follow, so it is left out and its file is read like any other.
fn mod_declarations(code: &str) -> Vec<ModDecl> {
    let c: Vec<char> = code.chars().collect();
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
    let mut found = Vec::new();
    let mut braces = 0usize;
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '{' => braces += 1,
            '}' => braces = braces.saturating_sub(1),
            _ => {}
        }
        let here = c.get(i..i + 3).is_some_and(|w| w == ['m', 'o', 'd']);
        if !here
            || braces > 0
            || (i > 0 && is_word(c[i - 1]))
            || c.get(i + 3).is_some_and(|ch| is_word(*ch))
        {
            i += 1;
            continue;
        }
        let name_at = skip_space(&c, i + 3);
        let name = word_at(&c, name_at);
        let after = skip_space(&c, name_at + name.len());
        if !name.is_empty() && c.get(after) == Some(&';') {
            found.push(ModDecl {
                name,
                path: path_attribute_before(&c, i),
            });
        }
        i += 3;
    }
    found
}

/// The file named by a `#[path = "…"]` attribute among the attributes and the
/// visibility just before the `mod` at `at`.
fn path_attribute_before(c: &[char], at: usize) -> Option<String> {
    let mut end = at;
    loop {
        let mut k = end;
        while k > 0 && c[k - 1].is_whitespace() {
            k -= 1;
        }
        if k > 0 && c[k - 1] == ']' {
            // An attribute: find its `[` and the `#` before it.
            let mut depth = 0i32;
            let mut open = k - 1;
            loop {
                match c[open] {
                    ']' => depth += 1,
                    '[' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                if open == 0 {
                    return None;
                }
                open -= 1;
            }
            let text: String = c[open + 1..k - 1].iter().collect();
            if let Some(rest) = text.trim().strip_prefix("path")
                && let Some(literal) = rest.trim_start().strip_prefix('=')
            {
                let literal = literal.trim();
                return literal
                    .strip_prefix('"')
                    .and_then(|l| l.strip_suffix('"'))
                    .map(str::to_owned);
            }
            end = open.saturating_sub(1);
            while end > 0 && c[end].is_whitespace() {
                end -= 1;
            }
            if c.get(end) != Some(&'#') {
                return None;
            }
        } else if k >= 3 && c[k - 3..k] == ['p', 'u', 'b'] {
            end = k - 3;
        } else {
            return None;
        }
    }
}

/// The file that `decl`, written in `from`, names, if it is one of `files`.
/// A module this cannot place is not found, and so is read like any other
/// file.
fn module_file(from: &Path, decl: &ModDecl, files: &BTreeSet<PathBuf>) -> Option<PathBuf> {
    let dir = from.parent()?;
    if let Some(path) = &decl.path {
        let file = dir.join(path);
        return files.contains(&file).then_some(file);
    }
    let own_folder = matches!(
        from.file_name().and_then(|n| n.to_str()),
        Some("lib.rs" | "main.rs" | "mod.rs")
    );
    let base = if own_folder {
        dir.to_path_buf()
    } else {
        dir.join(from.file_stem()?)
    };
    [
        base.join(format!("{}.rs", decl.name)),
        base.join(&decl.name).join("mod.rs"),
    ]
    .into_iter()
    .find(|file| files.contains(file))
}

/// The files of the crate in `dir` that are read: every file except those
/// that only a `#[cfg(test)]` module declaration brings in (and the files
/// those declare in turn). A file nobody declares is read.
fn files_to_read(dir: &Path) -> Vec<(PathBuf, String)> {
    let src = dir.join("src");
    if !src.is_dir() {
        return Vec::new();
    }
    let mut listed = Vec::new();
    rust_files(&src, &mut listed);
    let files: BTreeSet<PathBuf> = listed.into_iter().collect();
    let blanked: Vec<(PathBuf, String)> = files
        .iter()
        .map(|f| {
            (
                f.clone(),
                blank_non_code(&std::fs::read_to_string(f).unwrap()),
            )
        })
        .collect();
    let text_of = |f: &Path| &blanked.iter().find(|(p, _)| p == f).unwrap().1;
    // Declared anywhere, under any condition.
    let mut declared = BTreeSet::new();
    for (file, text) in &blanked {
        for decl in mod_declarations(text) {
            declared.extend(module_file(file, &decl, &files));
        }
    }
    // Declared by a file that is read, outside test-only items: also read.
    let mut read: BTreeSet<PathBuf> = files.difference(&declared).cloned().collect();
    loop {
        let mut added = false;
        for file in read.clone() {
            let production = without_test_items(text_of(&file));
            for decl in mod_declarations(&production) {
                if let Some(found) = module_file(&file, &decl, &files) {
                    added |= read.insert(found);
                }
            }
        }
        if !added {
            break;
        }
    }
    read.into_iter()
        .map(|file| {
            let text = without_test_items(text_of(&file));
            (file, text)
        })
        .collect()
}

fn violations_in(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for (file, code) in files_to_read(dir) {
        for word in hits(&code) {
            found.push(format!("{}: {word}", file.display()));
        }
    }
    found
}

#[test]
fn nothing_the_helper_links_can_start_a_program() {
    let mut dirs = BTreeSet::new();
    for target in [None, Some("x86_64-pc-windows-gnullvm")] {
        dirs.extend(closure_dirs(target));
    }
    // The closure includes the helper itself, and the crates this repository
    // links on each system.
    let names: BTreeSet<String> = dirs
        .iter()
        .filter_map(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    for expected in [
        "bytescape-helper",
        "bs-ipc",
        "bs-fsops",
        "bs-scan",
        "bs-tree",
        "bs-knowledge",
    ] {
        assert!(
            names.contains(expected),
            "{expected} missing from {names:?}"
        );
    }
    assert!(
        !names.contains("bs-platform"),
        "the helper must not link the platform services: {names:?}"
    );
    let mut all = Vec::new();
    for dir in &dirs {
        all.extend(violations_in(dir));
    }
    assert!(all.is_empty(), "code that can start programs: {all:#?}");
}

#[test]
fn the_check_sees_what_it_is_looking_for() {
    let code = code_of(
        "let c = std::process::Command::new(\"x\");\n// Command in a comment\nlet s = \"Command in a string\";\n#[cfg(test)]\nmod t { Command }\n",
    );
    assert_eq!(hits(&code), vec!["Command"]);
    let code = code_of("unsafe { libc::fork() }\nunsafe { CreateProcessW(a) }\n");
    assert_eq!(hits(&code), vec!["CreateProcessW", "fork"]);
    // Ordinary words, and longer names that merely contain a word, are fine.
    let code = code_of(
        "let forked = 1; fn systematic() {} fn system() {} let fork = 2; let c = x.clone(); fn execute() {}\nlet n = libc::getpid(); let v = libc::CLONE_NEWNS; fn fork_join() {}\n",
    );
    assert!(hits(&code).is_empty(), "{:?}", hits(&code));
}

#[test]
fn code_after_a_test_module_in_the_middle_of_a_file_is_still_read() {
    // A test module declared in another file: only that line is test code.
    let text = "fn a() {}\n#[cfg(test)]\nmod tests;\n\npub fn planted() {\n    std::process::Command::new(\"/bin/true\");\n}\n";
    assert_eq!(hits(&code_of(text)), vec!["Command"]);
    // A test module with its body in the file, followed by more code.
    let text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    use super::*;\n    fn t() { Command::new(\"x\"); }\n}\n\npub fn planted() {\n    let _ = std::process::Command::new(\"/bin/true\");\n}\n";
    assert_eq!(hits(&code_of(text)), vec!["Command"]);
    // The same, with the condition spelled with `all`, in either order, and
    // with further attributes in between.
    for attribute in [
        "#[cfg(all(test, windows))]",
        "#[cfg(all(unix, test))]",
        "#[cfg( test )]",
        "#[cfg(test)]\n#[allow(dead_code)]",
    ] {
        let text = format!(
            "{attribute}\nmod t {{ fn x() {{ Command::new(\"x\"); }} }}\nfn y() {{ std::process::Command::new(\"y\"); }}\n"
        );
        assert_eq!(hits(&code_of(&text)), vec!["Command"], "{attribute}");
        let text = format!("{attribute}\nmod t {{ fn x() {{ Command::new(\"x\"); }} }}\n");
        assert!(hits(&code_of(&text)).is_empty(), "{attribute}");
    }
    // An item that is not a module, and a semicolon item with brackets in it.
    let text = "#[cfg(test)]\nfn only_here(x: [u8; 2]) -> u8 { Command::new(\"x\"); 0 }\n#[cfg(test)]\nconst X: [u8; 2] = [0; 2];\nfn z() { Command::new(\"z\"); }\n";
    assert_eq!(hits(&code_of(text)), vec!["Command"]);
}

#[test]
fn conditions_that_can_hold_in_a_normal_build_are_not_test_code() {
    for attribute in [
        "#[cfg(not(test))]",
        "#[cfg(any(test, unix))]",
        "#[cfg(windows)]",
        "#[cfg(all(unix, not(test)))]",
        "#[cfg_attr(test, derive(Debug))]",
    ] {
        let text = format!("{attribute}\nfn f() {{ Command::new(\"x\"); }}\n");
        assert_eq!(hits(&code_of(&text)), vec!["Command"], "{attribute}");
    }
    // A file that is test code from its first line.
    assert!(hits(&code_of("#![cfg(test)]\nfn f() { Command::new(\"x\"); }\n")).is_empty());
}

#[test]
fn braces_and_quotes_in_strings_comments_and_literals_do_not_confuse_the_scan() {
    let text = r####"
#[cfg(test)]
mod tests {
    fn t() {
        let a = "} } }";
        let b = '}';
        let c = '"';
        let d = r#"} "quoted" }"#;
        // } not a brace
        /* } /* nested } */ } */
        let e: &'static str = "x";
        Command::new("test only");
    }
}

pub fn planted() {
    let _ = std::process::Command::new("/bin/true");
}
"####;
    assert_eq!(hits(&code_of(text)), vec!["Command"]);
    // The same names in comments, strings and literals alone are not code.
    let text = r####"
/* Command::new("x") /* nested Command */ still a comment Command */
// libc::fork()
/// A doc comment that mentions CreateProcessW.
fn f<'a>(x: &'a str) -> &'a str {
    let s = r#"Command::new("x")"#;
    let t = b"libc::fork";
    let u = '"';
    x
}
"####;
    assert!(
        hits(&code_of(text)).is_empty(),
        "{:?}",
        hits(&code_of(text))
    );
}

#[test]
fn system_call_numbers_and_the_c_library_ways_of_starting_a_program_are_seen() {
    for call in [
        "libc::syscall(libc::SYS_execve, a, b, c)",
        "libc::syscall(libc::SYS_execveat, a, b, c, d, e)",
        "libc::syscall(libc::SYS_fork)",
        "libc::syscall(libc::SYS_vfork)",
        "libc::syscall(libc::SYS_clone, 0, 0)",
        "libc::syscall(libc::SYS_clone3, a, 0)",
        "libc::clone(f, stack, flags, arg)",
        "libc::fork()",
        "libc::vfork()",
        "libc::posix_spawn(a, b, c, d, e, f)",
        "libc::posix_spawnp(a, b, c, d, e, f)",
        "libc::posix_spawn_file_actions_init(a)",
        "libc::execve(a, b, c)",
        "libc::execvpe(a, b, c)",
        "libc :: execl(a, b)",
        "libc::\n    fexecve(a, b, c)",
        "libc::system(a)",
    ] {
        assert!(!hits(&code_of(call)).is_empty(), "not seen: {call}");
    }
    // Imported by name, and declared by hand.
    for text in [
        "use libc::{getpid, fork};",
        "use libc::{\n    getpid,\n    execv,\n};",
        "use libc::vfork;",
        "extern \"C\" { fn fork() -> i32; }",
        "unsafe extern \"C\" {\n    fn execve(a: *const u8, b: *const u8, c: *const u8) -> i32;\n}",
    ] {
        assert!(!hits(&code_of(text)).is_empty(), "not seen: {text}");
    }
    // The calls the helper does make are not in the list.
    let code = code_of(
        "libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32); libc::syscall(libc::SYS_getdents64, fd);",
    );
    assert!(hits(&code).is_empty(), "{:?}", hits(&code));
}

#[test]
fn attributes_on_things_that_are_not_items_drop_only_that_thing() {
    // Each of these is followed by real code that starts a program. The
    // attribute applies to a field, variant, arm, parameter or literal field,
    // and what comes after is still read.
    let shapes = [
        (
            "a field",
            "pub struct S {\n    #[cfg(test)]\n    hook: u8,\n    b: u8,\n}\n\npub fn planted() {\n    let _ = std::process::Command::new(\"/bin/true\");\n}\n",
        ),
        (
            "a field with visibility",
            "pub struct S {\n    #[cfg(test)]\n    pub hook: u8,\n    pub b: u8,\n}\npub fn planted() { std::process::Command::new(\"x\"); }\n",
        ),
        (
            "a field with restricted visibility",
            "pub struct S { #[cfg(test)] pub(crate) hook: u8, b: u8 }\npub fn planted() { std::process::Command::new(\"x\"); }\n",
        ),
        (
            "the last field, without a comma",
            "pub struct S { b: u8, #[cfg(test)] hook: u8 }\npub fn planted() { std::process::Command::new(\"x\"); }\n",
        ),
        (
            "a tuple field",
            "pub struct T(#[cfg(test)] u8, u8);\npub fn planted() { std::process::Command::new(\"x\"); }\n",
        ),
        (
            "a variant",
            "pub enum E {\n    A,\n    #[cfg(test)]\n    T,\n}\npub fn planted() { let _ = std::process::Command::new(\"x\"); }\n",
        ),
        (
            "the last variant",
            "pub enum E { A, #[cfg(test)] T }\npub fn planted() { std::process::Command::new(\"x\"); }\n",
        ),
        (
            "a match arm",
            "fn f(x: u8) -> u8 {\n    match x {\n        #[cfg(test)]\n        0 => 1,\n        _ => { std::process::Command::new(\"x\"); 2 }\n    }\n}\n",
        ),
        (
            "the last match arm",
            "fn f(x: u8) { match x { _ => {}, #[cfg(test)] 0 => () } std::process::Command::new(\"x\"); }\n",
        ),
        (
            "a first parameter",
            "fn f(#[cfg(test)] a: u8, b: u8) { std::process::Command::new(\"x\"); }\n",
        ),
        (
            "a last parameter",
            "fn f(b: u8, #[cfg(test)] a: u8) { std::process::Command::new(\"x\"); }\n",
        ),
        (
            "a field of a struct literal",
            "fn f() -> S { S {\n    #[cfg(test)]\n    hook: 0,\n    b: 1 }\n}\npub fn planted() { std::process::Command::new(\"x\"); }\n",
        ),
        (
            "a statement",
            "fn f() {\n    #[cfg(test)]\n    check(a, b)\n}\npub fn planted() { std::process::Command::new(\"x\"); }\n",
        ),
    ];
    for (what, text) in shapes {
        assert_eq!(hits(&code_of(text)), vec!["Command"], "after {what}");
    }
}

#[test]
fn what_an_attribute_applies_to_is_dropped_and_nothing_else() {
    let mut dropped = Vec::new();
    let text = "pub struct S {\n    #[cfg(test)]\n    pub hook: Vec<(u8, u8)>,\n    pub b: u8,\n}\n#[cfg(test)]\nimpl<A, B> S<A, B> { fn t() { Command::new(\"t\"); } }\n#[cfg(test)]\npub(crate) fn g<A, B>(a: A, b: B) -> (A, B) { (a, b) }\nfn f(#[cfg(test)] a: u8) {}\n#[cfg(test)]\nconst X: [u8; 2] = [0; 2];\n#[cfg(test)]\nmod tests;\n";
    let kept = strip_test_items(&blank_non_code(text), &mut dropped);
    let dropped: Vec<String> = dropped
        .iter()
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(
        dropped,
        vec![
            "#[cfg(test)] pub hook: Vec<(u8, u8)>,",
            "#[cfg(test)] impl<A, B> S<A, B> { fn t() { Command::new( ); } }",
            "#[cfg(test)] pub(crate) fn g<A, B>(a: A, b: B) -> (A, B) { (a, b) }",
            "#[cfg(test)] a: u8",
            "#[cfg(test)] const X: [u8; 2] = [0; 2];",
            "#[cfg(test)] mod tests;",
        ]
    );
    assert!(kept.contains("pub b: u8") && kept.contains("fn f("));
    assert!(!kept.contains("Command"));
}

#[test]
fn a_libc_glob_or_alias_hides_nothing() {
    for text in [
        "use libc::*;\npub fn planted() { unsafe { fork(); } }\n",
        "use libc::{*};\n",
        "use libc::{getpid, *};\n",
        "use libc as c;\npub fn planted() { unsafe { c::fork(); } }\n",
        "extern crate libc as sys;\n",
        "use ::libc as c;\n",
        "use libc::{self as c};\n",
        "pub use libc::{unix::{self as c}};\n",
        "use libc ::\n    *;\n",
    ] {
        let found = hits(&code_of(text));
        assert!(
            found.iter().any(|h| h == "libc::*" || h == "libc as"),
            "not seen: {text}: {found:?}"
        );
    }
    // Plain uses of the library are not a glob or an alias.
    let code = code_of(
        "use libc::{self, c_int, getpid};\nlet n = libc::getpid(); let x = libc::c_int::MAX;\n",
    );
    assert!(hits(&code).is_empty(), "{:?}", hits(&code));
}

#[test]
fn byte_and_c_strings_are_read_like_strings() {
    for text in [
        "pub fn planted() { let _s = cr#\"\\\"#; std::process::Command::new(\"y\"); }\n",
        "pub fn planted() { let _s = c\"a \\\" b\"; std::process::Command::new(\"y\"); }\n",
        "pub fn planted() { let _s = br#\"\\\"#; std::process::Command::new(\"y\"); }\n",
        "pub fn planted() { let _s = b\"\\\\\"; std::process::Command::new(\"y\"); }\n",
        "pub fn planted() { let _s = cr##\"a\"#b\"##; std::process::Command::new(\"y\"); }\n",
    ] {
        assert_eq!(hits(&code_of(text)), vec!["Command"], "{text}");
    }
    // The same text inside a string is not code.
    let code = code_of("let a = cr#\"Command::new(\"x\")\"#; let b = c\"libc::fork\";\n");
    assert!(hits(&code).is_empty(), "{:?}", hits(&code));
}

#[test]
fn an_inner_test_condition_drops_its_own_scope_only() {
    // In an inline module: the module, and what comes after is read.
    let text = "mod m {\n    #![cfg(test)]\n    fn t() { Command::new(\"t\"); }\n}\npub fn planted() { std::process::Command::new(\"y\"); }\n";
    assert_eq!(hits(&code_of(text)), vec!["Command"]);
    // The same in a module inside a module.
    let text = "mod a { fn x() { Command::new(\"x\"); } mod b { #![cfg(all(test, unix))] fn t() { Command::new(\"t\"); } } }\nfn z() {}\n";
    assert_eq!(hits(&code_of(text)), vec!["Command"]);
    // At the top of the file: the whole file.
    let text = "#![cfg(test)]\nfn t() { Command::new(\"t\"); }\nmod m { fn u() { Command::new(\"u\"); } }\n";
    assert!(hits(&code_of(text)).is_empty());
    // Other inner attributes are not test conditions.
    let text = "#![allow(dead_code)]\nmod m {\n    #![allow(unused)]\n    fn t() { Command::new(\"t\"); }\n}\n";
    assert_eq!(hits(&code_of(text)), vec!["Command"]);
}

/// A crate directory with the given files under `src`.
struct ScratchCrate {
    root: PathBuf,
}

impl ScratchCrate {
    fn new(label: &str, files: &[(&str, &str)]) -> Self {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "bs-helper-nospawn-{label}-{}-{n}",
            std::process::id()
        ));
        for (name, text) in files {
            let path = root.join("src").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Self { root }
    }

    /// The names of the files with something to report, without their folder.
    fn reported(&self) -> Vec<String> {
        let src = self.root.join("src");
        violations_in(&self.root)
            .into_iter()
            .map(|line| {
                let (file, word) = line.rsplit_once(": ").unwrap();
                let file = Path::new(file).strip_prefix(&src).unwrap();
                // The same spelling on every system.
                format!("{}: {word}", file.display().to_string().replace('\\', "/"))
            })
            .collect()
    }
}

impl Drop for ScratchCrate {
    fn drop(&mut self) {
        // Only the folder this test made.
        if self
            .root
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("bs-helper-nospawn-"))
        {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

const SPAWN: &str = "pub fn s() { std::process::Command::new(\"x\"); }\n";

#[test]
fn a_planted_spawn_in_the_middle_of_a_crate_is_reported_by_file() {
    let planted = ScratchCrate::new(
        "planted",
        &[
            (
                "lib.rs",
                "pub fn a() {}\n#[cfg(test)]\nmod tests;\n\npub fn planted() {\n    let _ = std::process::Command::new(\"/bin/true\");\n}\n",
            ),
            ("tests.rs", SPAWN),
        ],
    );
    assert_eq!(planted.reported(), vec!["lib.rs: Command"]);

    let clean = ScratchCrate::new(
        "clean",
        &[
            ("lib.rs", "pub fn a() {}\n#[cfg(test)]\nmod tests;\n"),
            ("tests.rs", SPAWN),
        ],
    );
    assert!(clean.reported().is_empty());
}

#[test]
fn a_file_is_skipped_only_when_a_test_only_declaration_brings_it_in() {
    // The name does not matter: `tests.rs` declared without the condition is
    // production code, and a file with any other name is skipped when the
    // declaration says it is test code.
    let c = ScratchCrate::new(
        "declared",
        &[
            (
                "lib.rs",
                "mod tests;\n#[cfg(test)]\nmod fixtures;\n#[cfg(test)] mod proptests;\n",
            ),
            ("tests.rs", SPAWN),
            ("fixtures.rs", SPAWN),
            ("proptests.rs", SPAWN),
        ],
    );
    assert_eq!(c.reported(), vec!["tests.rs: Command"]);

    // A file that nothing declares is read, whatever it is called.
    let c = ScratchCrate::new(
        "orphan",
        &[
            ("lib.rs", "pub fn a() {}\n"),
            ("tests.rs", SPAWN),
            ("testutil.rs", SPAWN),
        ],
    );
    assert_eq!(
        c.reported(),
        vec!["tests.rs: Command", "testutil.rs: Command"]
    );

    // The files a test-only file declares are test-only too, and a file that
    // a read file declares outside test items is read.
    let c = ScratchCrate::new(
        "nested",
        &[
            (
                "lib.rs",
                "#[cfg(test)]\nmod tests;\nmod shared;\n#[cfg(test)]\nmod shared_too;\n",
            ),
            ("tests.rs", "mod inner;\n"),
            ("tests/inner.rs", SPAWN),
            ("shared.rs", SPAWN),
            ("shared_too.rs", SPAWN),
        ],
    );
    assert_eq!(c.reported(), vec!["shared.rs: Command"]);

    // A module declared in a folder, with a path attribute, and with a
    // declaration that is only a test condition among several.
    let c = ScratchCrate::new(
        "paths",
        &[
            ("lib.rs", "mod delete;\n"),
            (
                "delete.rs",
                "#[cfg(all(test, windows))]\n#[path = \"delete/tests_windows.rs\"]\nmod tests_windows;\n#[cfg(any(test, unix))]\nmod maybe;\n",
            ),
            ("delete/tests_windows.rs", SPAWN),
            ("delete/maybe.rs", SPAWN),
        ],
    );
    assert_eq!(c.reported(), vec!["delete/maybe.rs: Command"]);
}

#[test]
fn a_module_that_cannot_be_placed_is_read() {
    // A declaration inside an inline module is not followed, so the file is
    // not known to be test code.
    let c = ScratchCrate::new(
        "unplaced",
        &[
            ("lib.rs", "mod a {\n    #[cfg(test)]\n    mod t;\n}\n"),
            ("t.rs", SPAWN),
        ],
    );
    assert_eq!(c.reported(), vec!["t.rs: Command"]);
}

#[test]
fn a_checkout_inside_a_folder_called_tests_is_still_scanned() {
    let inner = ScratchCrate::new("nested-checkout", &[("lib.rs", SPAWN)]);
    let outer = inner.root.join("tests").join("checkout");
    std::fs::create_dir_all(outer.join("src")).unwrap();
    std::fs::write(outer.join("src/lib.rs"), SPAWN).unwrap();
    assert_eq!(violations_in(&outer).len(), 1);
}
