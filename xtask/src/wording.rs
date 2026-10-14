//! `cargo xtask wording`: Bytescape is a visualizer, not a cleanup advisor,
//! and it never claims space was freed when it was only moved to the Recycle
//! Bin. The shipped prose therefore must not use the banned words.
//!
//! The scan is case-insensitive and matches whole words. It covers the README,
//! the changelog (it becomes the release notes), the docs, the issue forms,
//! the package listings under `packaging/`, and any extra paths given on the
//! command line (for example the store listing text). A file in scope that is
//! not valid UTF-8 text is itself a finding, because it cannot be checked.
//!
//! The same rule also covers the words shown inside the program: string
//! literals in the interface crates and the knowledge tables' notes (see
//! [`crate::ui_wording`]).
//!
//! CONTRIBUTING, SECURITY and the pull request template are out of scope on
//! purpose: they have to name the banned words to explain the rule.

use crate::util::{Finding, relative, walk_files};
use std::path::{Path, PathBuf};

/// File types that hold prose people read. Other files in the prose folders
/// (screenshots, for example) are not text and are skipped.
const PROSE_EXTENSIONS: [&str; 11] = [
    "md", "txt", "html", "xml", "svg", "desktop", "yaml", "yml", "nuspec", "policy", "iss",
];

/// A file to check. `explicit` files were named by the caller, so they are
/// checked whatever their type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Covered {
    pub path: PathBuf,
    pub explicit: bool,
}

/// The banned words, whole and in any case. The one list: the scan below and
/// the comparison with the knowledge crate's own lint both read it.
pub const BANNED_WORDS: [&str; 3] = ["safe", "junk", "freed"];

/// Banned word stems, with the name used in messages: "reclaim" bans
/// "reclaimable" and "reclaiming" too.
pub const BANNED_STEMS: [(&str, &str); 1] = [("reclaim", "reclaim*")];

/// Banned two-word phrases (lower case), with the name used in messages.
pub const BANNED_PHRASES: [(&str, &str, &str); 1] = [("clean", "up", "clean up")];

/// The banned word found at the start of `words[index]`, if any.
fn banned_at(words: &[(usize, usize, String)], index: usize) -> Option<&'static str> {
    let word = words[index].2.as_str();
    if let Some(banned) = BANNED_WORDS.iter().find(|banned| **banned == word) {
        return Some(banned);
    }
    if let Some((_, name)) = BANNED_STEMS.iter().find(|(stem, _)| word.starts_with(stem)) {
        return Some(name);
    }
    let next = words.get(index + 1).map(|(_, _, text)| text.as_str());
    BANNED_PHRASES
        .iter()
        .find(|(first, second, _)| word == *first && next == Some(*second))
        .map(|(_, _, name)| *name)
}

/// Splits text into lowercase words (letters, digits, underscore) with their
/// 1-based line numbers and the position (in characters) where they start.
/// Line breaks inside a phrase do not hide it.
fn words_with_lines(text: &str) -> Vec<(usize, usize, String)> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut line = 1;
    let mut word_line = 1;
    let mut word_start = 0;
    for (count, character) in text.chars().enumerate() {
        if character.is_alphanumeric() || character == '_' {
            if current.is_empty() {
                word_line = line;
                word_start = count;
            }
            current.extend(character.to_lowercase());
        } else if !current.is_empty() {
            words.push((word_line, word_start, std::mem::take(&mut current)));
        }
        if character == '\n' {
            line += 1;
        }
    }
    if !current.is_empty() {
        words.push((word_line, word_start, current));
    }
    words
}

/// Banned words in one text, as `(line, word)` pairs.
pub fn scan_text(text: &str) -> Vec<(usize, &'static str)> {
    let words = words_with_lines(text);
    (0..words.len())
        .filter_map(|index| banned_at(&words, index).map(|word| (words[index].0, word)))
        .collect()
}

/// Banned words in one text, as `(position, word)` pairs, where the position
/// is the index of the word's first character.
pub fn scan_text_positions(text: &str) -> Vec<(usize, &'static str)> {
    let words = words_with_lines(text);
    (0..words.len())
        .filter_map(|index| banned_at(&words, index).map(|word| (words[index].1, word)))
        .collect()
}

fn is_prose(path: &Path) -> bool {
    let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    PROSE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// The files the scan covers below `root`, plus `extra` paths (files or
/// directories).
pub fn covered_files(root: &Path, extra: &[PathBuf]) -> Vec<Covered> {
    let mut files = Vec::new();
    for name in ["README.md", "CHANGELOG.md"] {
        let path = root.join(name);
        if path.is_file() {
            files.push(Covered {
                path,
                explicit: false,
            });
        }
    }
    for folder in ["docs", ".github/ISSUE_TEMPLATE", "packaging"] {
        for path in walk_files(&root.join(folder)) {
            if is_prose(&path) {
                files.push(Covered {
                    path,
                    explicit: false,
                });
            }
        }
    }
    for path in extra {
        let found = if path.is_dir() {
            walk_files(path)
        } else {
            vec![path.clone()]
        };
        files.extend(found.into_iter().map(|path| Covered {
            path,
            explicit: true,
        }));
    }
    files
}

/// Runs the check against the repository at `root`.
pub fn run(root: &Path, extra: &[PathBuf]) -> Result<Vec<Finding>, String> {
    if !root.join("README.md").is_file() {
        return Err(String::from(
            "README.md is missing, so there is no prose to check",
        ));
    }
    let mut findings = Vec::new();
    for covered in covered_files(root, extra) {
        let rel = relative(root, &covered.path);
        let bytes = std::fs::read(&covered.path)
            .map_err(|error| format!("could not read {rel}: {error}"))?;
        let Ok(text) = String::from_utf8(bytes) else {
            findings.push(Finding::new(
                rel,
                None,
                "is not valid UTF-8 text, so it cannot be checked (save it as UTF-8)",
            ));
            continue;
        };
        for (line, word) in scan_text(&text) {
            findings.push(Finding::new(
                rel.clone(),
                Some(line),
                format!("banned word \"{word}\""),
            ));
        }
    }
    findings.extend(crate::ui_wording::run(root)?);
    Ok(findings)
}
