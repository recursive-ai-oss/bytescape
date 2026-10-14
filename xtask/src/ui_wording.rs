//! The words people read inside the program: string literals in every crate,
//! and the notes in the knowledge tables. `cargo xtask wording` applies the
//! same banned-word rule to them as to the prose (see [`crate::wording`]).
//!
//! # What is scanned
//!
//! * Every string literal in `src/` of every crate under `crates/` (except
//!   those in [`SKIPPED_CRATES`], which is empty), found by
//!   a small Rust tokenizer (comments, character literals and lifetimes are
//!   skipped correctly; raw, byte and C strings and escape sequences are
//!   decoded, so `"Fre\u{65}d"` is still "Freed", and a word split by a
//!   backslash at the end of a line is joined as Rust joins it).
//! * Every string value in `crates/bs-knowledge/data/*.toml`. Comments in
//!   those files explain the rule and so name the banned words; they are not
//!   shown to anyone and are not scanned.
//!
//! # What is not scanned, and why
//!
//! * **Comments and doc comments.** They are never drawn on screen, and the
//!   safety comments and rule explanations have to be able to name the words.
//! * **Identifiers.** A variable called `safe_mode` is not text anyone reads.
//! * **Test code.** Items under `#[cfg(test)]` or `#[test]`, a file that is
//!   declared as a test module (`#[cfg(test)] mod tests;`), and everything
//!   outside `src/` (`tests/`, `benches/`, `examples/`) are not shipped.
//! * **Strings inside built-in attributes** such as `#[cfg(feature = "...")]`
//!   or `#[allow(..., reason = "...")]`: they configure the compiler. A string
//!   in any other attribute (for example an error-message derive) is scanned.
//!
//! Two limits are known. A phrase split across two literals (`concat!("clean ",
//! "up")`) is not seen as a phrase. A file pulled in with `include_str!` is
//! reported, because its words cannot be checked here.
//!
//! # Exceptions
//!
//! `xtask/wording.allow` lists exceptions, one per line: `path | word | text`.
//! An exception applies to a literal in that file that contains `text` and
//! has that banned word. An exception that matches nothing is itself a
//! finding, so the list cannot go stale. The product name "Disk Cleanup"
//! needs no exception: "cleanup" is one word, and only "clean up" is banned.

use crate::util::{Finding, parse_allow_lines, read_text, relative, walk_files};
use crate::wording::{BANNED_PHRASES, BANNED_STEMS, BANNED_WORDS, scan_text_positions};
use std::path::{Path, PathBuf};

/// Crates left out of the string scan, each with the reason. The scan reads
/// `src/` of every crate under `crates/`, so a new crate is covered from its
/// first commit. Adding a crate here needs a review; an entry that names a
/// crate that does not exist is a finding. Empty on purpose: every crate
/// passes today.
pub const SKIPPED_CRATES: [(&str, &str); 0] = [];

/// The knowledge tables, relative to the repository root.
pub const KNOWLEDGE_DATA: &str = "crates/bs-knowledge/data";

/// The reviewed list of exceptions, relative to the repository root.
pub const ALLOW_FILE: &str = "xtask/wording.allow";

/// A decoded string with, for every character, the line of the source it came
/// from. A line continuation drops characters from the text, so the line of a
/// word cannot be worked out from the text alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Literal {
    /// The line the literal starts on.
    pub line: usize,
    /// The decoded text.
    pub text: String,
    lines: Vec<usize>,
}

impl Literal {
    fn new(line: usize) -> Self {
        Self {
            line,
            text: String::new(),
            lines: Vec::new(),
        }
    }

    fn push(&mut self, c: char, line: usize) {
        self.text.push(c);
        self.lines.push(line);
    }

    /// The source line of the character at `index` (a character count, not a
    /// byte offset).
    pub fn line_of(&self, index: usize) -> usize {
        self.lines.get(index).copied().unwrap_or(self.line)
    }
}

/// A token of Rust source, as far as this scan needs to tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Punct(char),
    /// A string literal, with escapes decoded.
    Str(Literal),
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    tok: Tok,
    line: usize,
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    out: Vec<Token>,
}

impl Lexer {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek(0)?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn push(&mut self, tok: Tok, line: usize) {
        self.out.push(Token { tok, line });
    }

    /// After the opening quote of a normal string: reads and decodes it. Each
    /// decoded character remembers the source line it came from.
    fn cooked_string(&mut self) -> Literal {
        let mut lit = Literal::new(self.line);
        while let Some(c) = self.bump() {
            // `bump` has already counted a line break, so the line of `c`
            // itself is one less in that case.
            let at = if c == '\n' { self.line - 1 } else { self.line };
            match c {
                '"' => break,
                '\\' => match self.bump() {
                    Some('n') => lit.push('\n', at),
                    Some('r') => lit.push('\r', at),
                    Some('t') => lit.push('\t', at),
                    Some('0') => lit.push('\0', at),
                    Some('x') => {
                        let hex: String = (0..2).filter_map(|_| self.bump()).collect();
                        match u8::from_str_radix(&hex, 16) {
                            Ok(byte) => lit.push(char::from(byte), at),
                            Err(_) => lit.push(' ', at),
                        }
                    }
                    Some('u') => {
                        let mut hex = String::new();
                        if self.peek(0) == Some('{') {
                            self.bump();
                            while let Some(d) = self.bump() {
                                if d == '}' {
                                    break;
                                }
                                if d != '_' {
                                    hex.push(d);
                                }
                            }
                        }
                        match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                            Some(decoded) => lit.push(decoded, at),
                            None => lit.push(' ', at),
                        }
                    }
                    Some('\n') => {
                        // A backslash at the end of a line drops the break and
                        // all the white space that follows, as Rust does, so
                        // a word split across lines is one word again.
                        while matches!(self.peek(0), Some(' ' | '\t' | '\r' | '\n')) {
                            self.bump();
                        }
                    }
                    Some(other) => lit.push(other, at),
                    None => break,
                },
                other => lit.push(other, at),
            }
        }
        lit
    }

    /// After `r` (and any `b`/`c`), at the `#`s or the quote of a raw string.
    fn raw_string(&mut self, hashes: usize) -> Literal {
        // Skip the hashes and the opening quote.
        for _ in 0..=hashes {
            self.bump();
        }
        let mut lit = Literal::new(self.line);
        while let Some(c) = self.bump() {
            let at = if c == '\n' { self.line - 1 } else { self.line };
            if c == '"' && (0..hashes).all(|i| self.peek(i) == Some('#')) {
                for _ in 0..hashes {
                    self.bump();
                }
                break;
            }
            lit.push(c, at);
        }
        lit
    }

    fn run(mut self) -> Vec<Token> {
        while let Some(c) = self.peek(0) {
            let line = self.line;
            if c.is_whitespace() {
                self.bump();
            } else if c == '/' && self.peek(1) == Some('/') {
                while let Some(next) = self.peek(0) {
                    if next == '\n' {
                        break;
                    }
                    self.bump();
                }
            } else if c == '/' && self.peek(1) == Some('*') {
                self.bump();
                self.bump();
                let mut depth = 1;
                while depth > 0 {
                    match (self.peek(0), self.peek(1)) {
                        (Some('/'), Some('*')) => {
                            self.bump();
                            self.bump();
                            depth += 1;
                        }
                        (Some('*'), Some('/')) => {
                            self.bump();
                            self.bump();
                            depth -= 1;
                        }
                        (Some(_), _) => {
                            self.bump();
                        }
                        (None, _) => break,
                    }
                }
            } else if is_ident_start(c) {
                let mut ident = String::new();
                while let Some(next) = self.peek(0) {
                    if !is_ident_continue(next) {
                        break;
                    }
                    ident.push(next);
                    self.bump();
                }
                let raw_prefix = matches!(ident.as_str(), "r" | "br" | "cr");
                let cooked_prefix = matches!(ident.as_str(), "b" | "c");
                if raw_prefix && matches!(self.peek(0), Some('#' | '"')) {
                    let hashes = (0..).take_while(|&i| self.peek(i) == Some('#')).count();
                    if self.peek(hashes) == Some('"') {
                        let lit = self.raw_string(hashes);
                        self.push(Tok::Str(lit), line);
                        continue;
                    }
                    if ident == "r" && hashes == 1 {
                        // A raw identifier such as r#type.
                        self.bump();
                        let mut name = String::new();
                        while let Some(next) = self.peek(0) {
                            if !is_ident_continue(next) {
                                break;
                            }
                            name.push(next);
                            self.bump();
                        }
                        self.push(Tok::Ident(name), line);
                        continue;
                    }
                }
                if cooked_prefix && self.peek(0) == Some('"') {
                    self.bump();
                    let lit = self.cooked_string();
                    self.push(Tok::Str(lit), line);
                    continue;
                }
                self.push(Tok::Ident(ident), line);
            } else if c == '"' {
                self.bump();
                let lit = self.cooked_string();
                self.push(Tok::Str(lit), line);
            } else if c == '\'' {
                self.bump();
                if self.peek(0) == Some('\\') {
                    // An escaped character literal: skip to the closing quote.
                    self.bump();
                    self.bump();
                    while let Some(next) = self.bump() {
                        if next == '\'' {
                            break;
                        }
                    }
                    self.push(Tok::Other, line);
                } else if self.peek(1) == Some('\'') {
                    self.bump();
                    self.bump();
                    self.push(Tok::Other, line);
                } else {
                    // A lifetime or loop label.
                    while self.peek(0).is_some_and(is_ident_continue) {
                        self.bump();
                    }
                    self.push(Tok::Other, line);
                }
            } else if c.is_ascii_digit() {
                while self.peek(0).is_some_and(is_ident_continue) {
                    self.bump();
                }
                self.push(Tok::Other, line);
            } else {
                self.bump();
                self.push(Tok::Punct(c), line);
            }
        }
        self.out
    }
}

fn tokenize(text: &str) -> Vec<Token> {
    Lexer {
        chars: text.chars().collect(),
        pos: 0,
        line: 1,
        out: Vec::new(),
    }
    .run()
}

/// Attributes whose strings configure the compiler and are never shown.
const COMPILER_ATTRIBUTES: [&str; 22] = [
    "cfg",
    "cfg_attr",
    "doc",
    "allow",
    "expect",
    "warn",
    "deny",
    "forbid",
    "must_use",
    "deprecated",
    "path",
    "link",
    "link_name",
    "export_name",
    "no_mangle",
    "target_feature",
    "inline",
    "cold",
    "repr",
    "derive",
    "automatically_derived",
    "feature",
];

fn ident_is(token: Option<&Token>, name: &str) -> bool {
    matches!(token, Some(Token { tok: Tok::Ident(found), .. }) if found == name)
}

fn punct_is(token: Option<&Token>, c: char) -> bool {
    matches!(token, Some(Token { tok: Tok::Punct(found), .. }) if *found == c)
}

/// Is this attribute (the tokens between `[` and `]`) a test-only `cfg`?
/// `cfg(test)` and `cfg(all(test, ...))` are; `cfg(any(test, ...))` and
/// `cfg(not(test))` also build outside tests, so they are not.
fn is_test_cfg(attr: &[Token]) -> bool {
    if !ident_is(attr.first(), "cfg") {
        return false;
    }
    let idents: Vec<&str> = attr
        .iter()
        .filter_map(|token| match &token.tok {
            Tok::Ident(name) => Some(name.as_str()),
            _ => None,
        })
        .collect();
    let allowed_shape = matches!(idents.get(1), Some(&"test") | Some(&"all"));
    allowed_shape
        && idents.contains(&"test")
        && !idents.contains(&"not")
        && !idents.contains(&"any")
}

/// Index just after the item that starts at `start`: it ends at the `;` or at
/// the closing `}` that brings the nesting back to zero.
fn skip_item(tokens: &[Token], start: usize) -> usize {
    let mut depth = 0usize;
    let mut index = start;
    while let Some(token) = tokens.get(index) {
        if let Tok::Punct(c) = token.tok {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' => depth = depth.saturating_sub(1),
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return index + 1;
                    }
                }
                ';' if depth == 0 => return index + 1,
                _ => {}
            }
        }
        index += 1;
    }
    index
}

/// What one Rust file holds.
#[derive(Debug, Default)]
pub struct SourceScan {
    /// Non-test string literals as `(line, decoded text)`.
    pub strings: Vec<Literal>,
    /// Lines that pull in text from another file with `include_str!`.
    pub includes: Vec<usize>,
    /// Names of modules declared `#[cfg(test)] mod name;` (their files are
    /// test code).
    pub test_modules: Vec<String>,
    /// The whole file is test code (`#![cfg(test)]`).
    pub all_test: bool,
}

/// Finds the non-test string literals in Rust source.
pub fn scan_source(text: &str) -> SourceScan {
    let tokens = tokenize(text);
    let mut scan = SourceScan::default();
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        match &token.tok {
            Tok::Punct('#') => {
                let mut open = index + 1;
                let inner = punct_is(tokens.get(open), '!');
                if inner {
                    open += 1;
                }
                if !punct_is(tokens.get(open), '[') {
                    index += 1;
                    continue;
                }
                // Find the matching `]`.
                let mut depth = 0usize;
                let mut close = open;
                for (offset, inside) in tokens.iter().enumerate().skip(open) {
                    match inside.tok {
                        Tok::Punct('[') => depth += 1,
                        Tok::Punct(']') => {
                            depth -= 1;
                            if depth == 0 {
                                close = offset;
                                break;
                            }
                        }
                        _ => {}
                    }
                    close = offset;
                }
                let attr = &tokens[open + 1..close.max(open + 1)];
                let test_cfg = is_test_cfg(attr);
                let test_fn = attr.len() == 1 && ident_is(attr.first(), "test");
                if inner && test_cfg {
                    scan.all_test = true;
                    return scan;
                }
                if !inner && (test_cfg || test_fn) {
                    let end = skip_item(&tokens, close + 1);
                    let item = &tokens[close + 1..end];
                    if test_cfg
                        && let Some(at) = item.iter().position(|t| ident_is(Some(t), "mod"))
                        && let Some(Token {
                            tok: Tok::Ident(name),
                            ..
                        }) = item.get(at + 1)
                        && punct_is(item.get(at + 2), ';')
                    {
                        scan.test_modules.push(name.clone());
                    }
                    index = end;
                    continue;
                }
                let configures_compiler = matches!(
                    attr.first(),
                    Some(Token { tok: Tok::Ident(name), .. })
                        if COMPILER_ATTRIBUTES.contains(&name.as_str())
                );
                if !configures_compiler {
                    for inside in attr {
                        if let Tok::Str(lit) = &inside.tok {
                            scan.strings.push(lit.clone());
                        }
                    }
                }
                index = close + 1;
            }
            Tok::Str(lit) => {
                scan.strings.push(lit.clone());
                index += 1;
            }
            Tok::Ident(name) if name == "include_str" && punct_is(tokens.get(index + 1), '!') => {
                scan.includes.push(token.line);
                index += 1;
            }
            _ => index += 1,
        }
    }
    scan
}

/// The string values in a TOML file. Comments are skipped.
pub fn toml_strings(text: &str) -> Vec<Literal> {
    let chars: Vec<char> = text.chars().collect();
    let mut found = Vec::new();
    let mut line = 1;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let at = |n: usize| chars.get(i + n).copied();
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '"' | '\'' => {
                let multi = at(1) == Some(c) && at(2) == Some(c);
                let mut lit = Literal::new(line);
                i += if multi { 3 } else { 1 };
                while i < chars.len() {
                    let d = chars[i];
                    if d == '\n' && !multi {
                        // An unterminated string ends at the line break.
                        break;
                    }
                    let closes = if multi {
                        d == c && chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c)
                    } else {
                        d == c
                    };
                    if closes {
                        i += if multi { 3 } else { 1 };
                        break;
                    }
                    if d == '\\' && c == '"' {
                        let escape_line = line;
                        i += 1;
                        match chars.get(i) {
                            Some('n') => lit.push('\n', escape_line),
                            Some('t') => lit.push('\t', escape_line),
                            Some(&esc @ ('u' | 'U')) => {
                                // \uXXXX and \UXXXXXXXX
                                let digits = if esc == 'u' { 4 } else { 8 };
                                let hex: String = chars.iter().skip(i + 1).take(digits).collect();
                                if let Some(decoded) =
                                    u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                                {
                                    lit.push(decoded, escape_line);
                                }
                                i += digits;
                            }
                            Some(' ' | '\t' | '\r' | '\n') if multi => {
                                // A backslash at the end of a line in a multi-line
                                // string drops the break and all the white space
                                // after it. A backslash followed by other text on
                                // the line is not valid; keep what is there.
                                let mut ahead = i;
                                while matches!(chars.get(ahead), Some(' ' | '\t' | '\r')) {
                                    ahead += 1;
                                }
                                if chars.get(ahead) == Some(&'\n') {
                                    while matches!(chars.get(i), Some(' ' | '\t' | '\r' | '\n')) {
                                        if chars[i] == '\n' {
                                            line += 1;
                                        }
                                        i += 1;
                                    }
                                    // `i` now points at the next character,
                                    // which the loop must read, not skip.
                                    continue;
                                }
                                lit.push(chars[i], escape_line);
                            }
                            Some(other) => lit.push(*other, escape_line),
                            None => {}
                        }
                        i += 1;
                        continue;
                    }
                    if d == '\n' {
                        line += 1;
                    }
                    lit.push(d, if d == '\n' { line - 1 } else { line });
                    i += 1;
                }
                found.push(lit);
            }
            _ => i += 1,
        }
    }
    found
}

/// A banned word in one literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub rel: String,
    pub line: usize,
    pub word: &'static str,
    pub literal: String,
}

/// One reviewed exception from `xtask/wording.allow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exception {
    pub path: String,
    pub word: String,
    pub text: String,
}

/// Parses the exception list.
pub fn parse_exceptions(text: &str) -> Result<Vec<Exception>, String> {
    parse_allow_lines(text)
        .into_iter()
        .map(|line| {
            let parts: Vec<&str> = line.splitn(3, '|').map(str::trim).collect();
            match parts.as_slice() {
                [path, word, literal]
                    if !path.is_empty() && !word.is_empty() && !literal.is_empty() =>
                {
                    Ok(Exception {
                        path: (*path).to_string(),
                        word: (*word).to_string(),
                        text: (*literal).to_string(),
                    })
                }
                _ => Err(format!(
                    "{ALLOW_FILE}: `{line}` is not `path | word | text` (all three parts are needed)"
                )),
            }
        })
        .collect()
}

fn hits_in_literals(rel: &str, literals: &[Literal]) -> Vec<Hit> {
    let mut hits = Vec::new();
    for literal in literals {
        for (position, word) in scan_text_positions(&literal.text) {
            hits.push(Hit {
                rel: rel.to_string(),
                line: literal.line_of(position),
                word,
                literal: literal.text.clone(),
            });
        }
    }
    hits
}

/// The directory that holds the files of the modules declared in `file`.
fn module_dir(file: &Path) -> PathBuf {
    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(stem, "lib" | "main" | "mod") {
        dir
    } else {
        dir.join(stem)
    }
}

/// Findings that are not banned words: files that cannot be checked.
#[derive(Debug, Default)]
pub struct CrateScan {
    pub hits: Vec<Hit>,
    pub problems: Vec<Finding>,
}

/// Scans the `src/` of one crate directory.
pub fn scan_crate(root: &Path, crate_dir: &Path) -> CrateScan {
    let mut result = CrateScan::default();
    let mut scans = Vec::new();
    let mut excluded: Vec<PathBuf> = Vec::new();
    for path in walk_files(&crate_dir.join("src")) {
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let rel = relative(root, &path);
        let Some(text) = read_text(&path) else {
            result.problems.push(Finding::new(
                rel,
                None,
                "is not valid UTF-8 text, so its strings cannot be checked",
            ));
            continue;
        };
        let scan = scan_source(&text);
        let dir = module_dir(&path);
        for name in &scan.test_modules {
            excluded.push(dir.join(format!("{name}.rs")));
            excluded.push(dir.join(name));
        }
        scans.push((path, rel, scan));
    }
    for (path, rel, scan) in scans {
        if scan.all_test || excluded.iter().any(|skip| path.starts_with(skip)) {
            continue;
        }
        for line in &scan.includes {
            result.problems.push(Finding::new(
                rel.clone(),
                Some(*line),
                "include_str! pulls in text that this check does not read; write the text as a literal here, or list the file for the check",
            ));
        }
        result.hits.extend(hits_in_literals(&rel, &scan.strings));
    }
    result
}

/// Scans the knowledge tables.
pub fn scan_knowledge_data(root: &Path) -> CrateScan {
    let mut result = CrateScan::default();
    for path in walk_files(&root.join(KNOWLEDGE_DATA)) {
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let rel = relative(root, &path);
        match read_text(&path) {
            Some(text) => result
                .hits
                .extend(hits_in_literals(&rel, &toml_strings(&text))),
            None => result.problems.push(Finding::new(
                rel,
                None,
                "is not valid UTF-8 text, so its notes cannot be checked",
            )),
        }
    }
    result
}

/// Removes the hits covered by an exception. Returns the remaining hits and
/// the exceptions that matched nothing.
pub fn apply_exceptions(hits: Vec<Hit>, exceptions: &[Exception]) -> (Vec<Hit>, Vec<Exception>) {
    let mut used = vec![false; exceptions.len()];
    let mut remaining = Vec::new();
    for hit in hits {
        let matching = exceptions.iter().position(|exception| {
            exception.path == hit.rel
                && exception.word == hit.word
                && hit.literal.contains(&exception.text)
        });
        match matching {
            Some(index) => used[index] = true,
            None => remaining.push(hit),
        }
    }
    let unused = exceptions
        .iter()
        .zip(used)
        .filter(|(_, used)| !used)
        .map(|(exception, _)| exception.clone())
        .collect();
    (remaining, unused)
}

/// The string literals of `const NAME: ... = <expression>;` in Rust source:
/// the table's entries. Comments are not tokens, so a commented-out entry is
/// not one.
fn const_strings(tokens: &[Token], name: &str) -> Vec<String> {
    let Some(at) = tokens
        .windows(2)
        .position(|pair| ident_is(pair.first(), "const") && ident_is(pair.get(1), name))
    else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut depth = 0usize;
    let mut in_value = false;
    for token in &tokens[at..] {
        match &token.tok {
            Tok::Punct('=') if !in_value => in_value = true,
            Tok::Punct('(' | '[' | '{') if in_value => depth += 1,
            Tok::Punct(')' | ']' | '}') if in_value => depth = depth.saturating_sub(1),
            Tok::Punct(';') if in_value && depth == 0 => break,
            Tok::Str(lit) if in_value => found.push(lit.text.to_lowercase()),
            _ => {}
        }
    }
    found
}

/// Checks that the knowledge crate's own note lint bans everything this scan
/// bans. Returns what its lint is missing. `lint_source` is the text of
/// `crates/bs-knowledge/src/lint.rs`; only the string literals in its `WORDS`,
/// `STEMS` and `PHRASES` tables count.
pub fn knowledge_lint_gaps(lint_source: &str) -> Vec<String> {
    let tokens = tokenize(lint_source);
    let words = const_strings(&tokens, "WORDS");
    let stems = const_strings(&tokens, "STEMS");
    let phrases = const_strings(&tokens, "PHRASES");
    let mut gaps = Vec::new();
    for banned in BANNED_WORDS {
        if !words.iter().any(|word| word == banned) {
            gaps.push(banned.to_string());
        }
    }
    for (stem, name) in BANNED_STEMS {
        if !stems.iter().any(|found| found == stem) {
            gaps.push(name.to_string());
        }
    }
    for (first, second, name) in BANNED_PHRASES {
        let present = phrases
            .windows(2)
            .any(|pair| pair[0] == first && pair[1] == second);
        if !present {
            gaps.push(name.to_string());
        }
    }
    gaps
}

/// The directories under `crates/`, sorted.
fn crate_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root.join("crates"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs
}

/// The whole UI-strings check below `root`.
pub fn run(root: &Path) -> Result<Vec<Finding>, String> {
    let exceptions = match std::fs::read_to_string(root.join(ALLOW_FILE)) {
        Ok(text) => parse_exceptions(&text)?,
        Err(_) => Vec::new(),
    };
    let mut findings = Vec::new();
    let mut hits = Vec::new();
    let dirs = crate_dirs(root);
    let has_crates = root.join("crates").is_dir();
    for (name, _) in SKIPPED_CRATES {
        if has_crates
            && !dirs
                .iter()
                .any(|dir| dir.file_name().is_some_and(|n| n == name))
        {
            findings.push(Finding::new(
                format!("crates/{name}"),
                None,
                "is in SKIPPED_CRATES but does not exist; remove the entry from xtask/src/ui_wording.rs",
            ));
        }
    }
    for dir in dirs {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if SKIPPED_CRATES.iter().any(|(skipped, _)| *skipped == name) {
            continue;
        }
        let scan = scan_crate(root, &dir);
        hits.extend(scan.hits);
        findings.extend(scan.problems);
    }
    let data = scan_knowledge_data(root);
    hits.extend(data.hits);
    findings.extend(data.problems);

    let (hits, unused) = apply_exceptions(hits, &exceptions);
    for hit in hits {
        findings.push(Finding::new(
            hit.rel,
            Some(hit.line),
            format!(
                "banned word \"{}\" in text shown to people: {:?}",
                hit.word,
                excerpt(&hit.literal)
            ),
        ));
    }
    for exception in unused {
        findings.push(Finding::new(
            ALLOW_FILE,
            None,
            format!(
                "exception `{} | {} | {}` matches no text; remove it",
                exception.path, exception.word, exception.text
            ),
        ));
    }

    // The knowledge crate's own test applies a stricter list to the notes.
    // Its list must still contain every word this check bans.
    let knowledge_manifest = root.join("crates/bs-knowledge/Cargo.toml");
    if knowledge_manifest.is_file() {
        match read_text(&root.join("crates/bs-knowledge/src/lint.rs")) {
            Some(source) => {
                for gap in knowledge_lint_gaps(&source) {
                    findings.push(Finding::new(
                        "crates/bs-knowledge/src/lint.rs",
                        None,
                        format!("does not ban \"{gap}\", which the wording rule bans"),
                    ));
                }
            }
            None => findings.push(Finding::new(
                "crates/bs-knowledge/src/lint.rs",
                None,
                "is missing, so the crate's own note lint cannot be compared with the wording rule",
            )),
        }
    }
    Ok(findings)
}

/// A short piece of a literal for messages.
fn excerpt(text: &str) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= 80 {
        one_line
    } else {
        let head: String = one_line.chars().take(77).collect();
        format!("{head}...")
    }
}
