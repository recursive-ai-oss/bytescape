//! `cargo xtask public-text`: everything in the public repository is public,
//! so no shipped file (code comments, workflow files and generated output
//! included) may mention the private working material: decision, rule, gate
//! or phase ids, the private planning files, process-role vocabulary, unit
//! numbers, or commit hashes.
//!
//! The generic shapes are built in. Names that are specific to the private
//! material are kept out of this public repository and loaded from a file
//! given with `--extra-patterns` (see [`Extra`]).
//!
//! Where a rule shaped a file, a comment gives the reason in plain words
//! instead. Pinned action versions in workflow files are the one place a full
//! commit hash is expected.

mod patterns;

use crate::util::{Finding, read_text, relative, walk_files};
use patterns::{
    AGENT_OK_AFTER, AGENT_WORD, EXEMPT_PATH, PRIVATE_FILE_WORDS, PROCESS_WORDS, WORD_ENDINGS,
};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Extra patterns loaded from a file at run time, for names that must not be
/// written down in this repository. One entry per line; `#` starts a comment:
///
/// ```text
/// word TOKEN        # a whole word, any case
/// substring TEXT    # a piece of text, any case
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Extra {
    pub words: Vec<String>,
    pub substrings: Vec<String>,
}

impl Extra {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut extra = Self::default();
        for (index, line) in text.lines().enumerate() {
            let entry = line.split('#').next().unwrap_or("").trim();
            if entry.is_empty() {
                continue;
            }
            match entry.split_once(char::is_whitespace) {
                Some(("word", token)) if !token.trim().is_empty() => {
                    extra.words.push(token.trim().to_lowercase());
                }
                Some(("substring", token)) if !token.trim().is_empty() => {
                    extra.substrings.push(token.trim().to_lowercase());
                }
                _ => {
                    return Err(format!(
                        "line {}: expected `word TOKEN` or `substring TEXT`",
                        index + 1
                    ));
                }
            }
        }
        Ok(extra)
    }
}

/// Dashes and minus signs that can stand in for a hyphen.
fn is_dash(character: char) -> bool {
    matches!(character, '-' | '\u{2010}'..='\u{2015}' | '\u{2212}')
}

fn is_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// Words of a line as `(start byte, text)`.
fn words(line: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut start = None;
    for (index, character) in line.char_indices() {
        match (is_word_char(character), start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                found.push((begin, &line[begin..index]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        found.push((begin, &line[begin..]));
    }
    found
}

fn digits_in_range(text: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&text.len()) && text.chars().all(|c| c.is_ascii_digit())
}

/// A decision id: the letter Q and one or two digits.
fn is_decision_id(word: &str) -> bool {
    word.strip_prefix('Q')
        .is_some_and(|rest| digits_in_range(rest, 1, 2))
}

/// A gate id: the letter G and a digit from 1 to 7.
fn is_gate_id(word: &str) -> bool {
    matches!(
        word.strip_prefix('G'),
        Some("1" | "2" | "3" | "4" | "5" | "6" | "7")
    )
}

/// A phase id: the letter P and a digit from 0 to 6.
fn is_phase_id(word: &str) -> bool {
    matches!(
        word.strip_prefix('P'),
        Some("0" | "1" | "2" | "3" | "4" | "5" | "6")
    )
}

/// A run of hex digits that looks like a commit hash: 7 to 40 characters with
/// at least one digit and one letter, standing alone.
fn find_commit_hash(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if !chars[index].is_ascii_hexdigit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && chars[index].is_ascii_hexdigit() {
            index += 1;
        }
        let run = &chars[start..index];
        let before = start.checked_sub(1).map(|i| chars[i]);
        let after = chars.get(index).copied();
        let joined = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || "_-.".contains(c));
        let has_digit = run.iter().any(char::is_ascii_digit);
        let has_letter = run.iter().any(char::is_ascii_alphabetic);
        if (7..=40).contains(&run.len())
            && has_digit
            && has_letter
            && !joined(before)
            && !joined(after)
        {
            return true;
        }
    }
    false
}

/// All problems on one line, as short descriptions.
pub fn scan_line(line: &str, extra: &Extra) -> Vec<String> {
    let mut problems = Vec::new();
    let found = words(line);
    for (position, &(start, word)) in found.iter().enumerate() {
        let lower = word.to_lowercase();
        let rest = &line[start + word.len()..];
        if is_decision_id(word) {
            problems.push(format!("decision id \"{word}\""));
        }
        if is_gate_id(word) {
            problems.push(format!("gate id \"{word}\""));
        }
        if is_phase_id(word) {
            problems.push(format!("phase id \"{word}\""));
        }
        let after_dash = rest.strip_prefix(is_dash);
        if word == "NN" && after_dash.is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit())) {
            problems.push(String::from("rule id \"NN-<number>\""));
        }
        // A unit id: lowercase p, a digit, a dash and a name.
        let is_unit_id = lower.len() == 2
            && lower.starts_with('p')
            && lower[1..].chars().all(|c| ('0'..='6').contains(&c))
            && after_dash.is_some_and(|r| r.starts_with(|c: char| c.is_ascii_lowercase()));
        if is_unit_id {
            problems.push(format!("unit id \"{word}-...\""));
        }
        let number_follows = rest.starts_with(char::is_whitespace)
            && rest.trim_start().starts_with(|c: char| c.is_ascii_digit());
        if lower == "unit" && number_follows {
            problems.push(String::from("unit number"));
        }
        let process_word = PROCESS_WORDS.iter().any(|base| {
            lower
                .strip_prefix(base)
                .is_some_and(|ending| WORD_ENDINGS.contains(&ending))
        });
        if process_word || extra.words.contains(&lower) {
            problems.push(format!("private-tooling word \"{word}\""));
        }
        let needs_context = lower
            .strip_prefix(AGENT_WORD)
            .is_some_and(|ending| WORD_ENDINGS.contains(&ending));
        if needs_context {
            let before = position.checked_sub(1).map(|i| found[i].1.to_lowercase());
            let fine = before.is_some_and(|b| AGENT_OK_AFTER.contains(&b.as_str()));
            if !fine {
                problems.push(format!("private-tooling word \"{word}\""));
            }
        }
        if PRIVATE_FILE_WORDS.contains(&word) {
            problems.push(format!("private planning file name \"{word}\""));
        }
    }
    let lower_line = line.to_lowercase();
    for needle in &extra.substrings {
        if lower_line.contains(needle.as_str()) {
            problems.push(format!("private path or repository \"{needle}\""));
        }
    }
    // A section sign followed by a number points into a private document.
    let mut after_sign = line.split('\u{a7}').skip(1);
    if after_sign.any(|part| part.trim_start().starts_with(|c: char| c.is_ascii_digit())) {
        problems.push(String::from("section reference to a private document"));
    }
    // Pinned actions carry a full commit hash by design.
    if !line.contains("uses:") && find_commit_hash(line) {
        problems.push(String::from("commit hash"));
    }
    problems
}

/// All problems in a text, as `(line, description)`.
pub fn scan_text(text: &str, extra: &Extra) -> Vec<(usize, String)> {
    let mut problems = Vec::new();
    for (index, line) in text.lines().enumerate() {
        for problem in scan_line(line, extra) {
            problems.push((index + 1, problem));
        }
    }
    problems
}

/// The files to scan: what git tracks or would add (so ignored files are
/// left out), or every file below `root` when it is not a git checkout.
pub fn candidate_files(root: &Path) -> Vec<PathBuf> {
    // Ask git only when `root` is itself a checkout; a directory inside some
    // other repository would otherwise be filtered by that repository's rules.
    let listed = root.join(".git").exists().then(|| {
        Command::new("git")
            .current_dir(root)
            .args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ])
            .output()
    });
    match listed {
        Some(Ok(output)) if output.status.success() => {
            let mut files: Vec<PathBuf> = output
                .stdout
                .split(|&byte| byte == 0)
                .filter(|name| !name.is_empty())
                .map(|name| root.join(String::from_utf8_lossy(name).as_ref()))
                .filter(|path| path.is_file())
                .collect();
            files.sort();
            files.dedup();
            files
        }
        _ => walk_files(root),
    }
}

/// Runs the check against the repository at `root`.
pub fn run(root: &Path, extra: &Extra) -> Result<Vec<Finding>, String> {
    let mut findings = Vec::new();
    let files = candidate_files(root);
    if files.is_empty() {
        return Err(format!("no files found to scan below {}", root.display()));
    }
    for path in files {
        let rel = relative(root, &path);
        if rel == EXEMPT_PATH {
            continue;
        }
        let Some(text) = read_text(&path) else {
            continue;
        };
        for (line, problem) in scan_text(&text, extra) {
            findings.push(Finding::new(rel.clone(), Some(line), problem));
        }
    }
    Ok(findings)
}
