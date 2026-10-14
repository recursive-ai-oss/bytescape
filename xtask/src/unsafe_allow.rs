//! `cargo xtask unsafe-allow`: `unsafe` code is denied across the workspace,
//! and may be allowed only in the crates that talk to the operating system or
//! read raw disk structures. Every `unsafe` block there still needs a
//! `// SAFETY:` comment, which a workspace lint enforces; this check keeps
//! that lint, and the deny rule itself, from being switched off.
//!
//! It fails when:
//! - an attribute allows `unsafe_code` in a source file of any other crate,
//!   or in the repository's other Rust files (xtask, packaging);
//! - a crate manifest other than those crates mentions `unsafe_code`;
//! - any attribute anywhere allows the missing-safety-comment lint or the
//!   `unsafe_op_in_unsafe_fn` lint;
//! - the root manifest no longer sets `unsafe_code = "deny"`.

use crate::util::{Finding, read_text, relative, walk_files};
use std::path::Path;

/// The crates that may allow `unsafe_code`, with the reason.
pub const MAY_ALLOW_UNSAFE: [(&str, &str); 8] = [
    (
        "bytescape-helper",
        "elevated helper: operating system calls",
    ),
    (
        "bs-platform",
        "platform services: operating system bindings",
    ),
    (
        "bs-scan",
        "walker: directory-handle and getdents/statx calls",
    ),
    ("bs-scan-ntfs", "raw NTFS reader"),
    ("bs-scan-ext4", "raw ext4 reader"),
    ("bs-scan-btrfs", "btrfs ioctl reader"),
    ("bs-scan-xfs", "XFS ioctl reader"),
    ("bs-render", "software surface bindings, if needed"),
];

/// Lints that no attribute may switch off, in any crate.
const NEVER_RELAXED: [&str; 2] = ["undocumented_unsafe_blocks", "unsafe_op_in_unsafe_fn"];

const UNSAFE_LINT: &str = "unsafe_code";

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Contents of every `allow(...)` or `expect(...)` in the text, with the
/// 1-based line where it starts.
fn relaxations(text: &str) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let starts_word =
            is_ident_byte(bytes[index]) && (index == 0 || !is_ident_byte(bytes[index - 1]));
        if !starts_word {
            index += 1;
            continue;
        }
        let mut end = index;
        while end < bytes.len() && is_ident_byte(bytes[end]) {
            end += 1;
        }
        let word = &text[index..end];
        if word == "allow" || word == "expect" {
            let mut open = end;
            while open < bytes.len() && bytes[open].is_ascii_whitespace() {
                open += 1;
            }
            if bytes.get(open) == Some(&b'(') {
                let mut depth = 0usize;
                let mut close = open;
                while close < bytes.len() {
                    match bytes[close] {
                        b'(' => depth += 1,
                        b')' => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    close += 1;
                }
                let line = 1 + bytes[..index].iter().filter(|&&b| b == b'\n').count();
                found.push((line, text[open..close.min(bytes.len())].to_string()));
            }
        }
        index = end;
    }
    found
}

/// Whole-identifier test for a lint name inside attribute contents.
fn mentions(contents: &str, lint: &str) -> bool {
    contents
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|part| part == lint)
}

/// The crate directory name for a path like `crates/<name>/...`.
fn crate_of(rel: &str) -> Option<&str> {
    rel.strip_prefix("crates/")?.split('/').next()
}

fn may_allow(rel: &str) -> bool {
    crate_of(rel).is_some_and(|name| MAY_ALLOW_UNSAFE.iter().any(|(allowed, _)| *allowed == name))
}

/// Checks one Rust source file's text.
pub fn check_source(rel: &str, text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (line, contents) in relaxations(text) {
        if mentions(&contents, UNSAFE_LINT) && !may_allow(rel) {
            findings.push(Finding::new(
                rel,
                Some(line),
                "allows unsafe code outside the crates that may (see xtask/src/unsafe_allow.rs)",
            ));
        }
        for lint in NEVER_RELAXED {
            if mentions(&contents, lint) {
                findings.push(Finding::new(
                    rel,
                    Some(line),
                    format!("switches off the `{lint}` lint, which no crate may do"),
                ));
            }
        }
    }
    findings
}

/// Checks one manifest's text.
pub fn check_manifest(rel: &str, text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    if rel == "Cargo.toml" {
        let denies = text.lines().any(|line| {
            let line = line.split('#').next().unwrap_or("").replace(' ', "");
            line == format!("{UNSAFE_LINT}=\"deny\"")
        });
        if !denies {
            findings.push(Finding::new(
                rel,
                None,
                "the workspace must keep `unsafe_code = \"deny\"`",
            ));
        }
    } else if crate_of(rel).is_some() && !may_allow(rel) {
        for (index, line) in text.lines().enumerate() {
            let code = line.split('#').next().unwrap_or("");
            if mentions(code, UNSAFE_LINT) {
                findings.push(Finding::new(
                    rel,
                    Some(index + 1),
                    "sets the unsafe code lint in a crate that may not allow it",
                ));
            }
        }
    }
    findings
}

/// Runs the check against the repository at `root`.
pub fn run(root: &Path) -> Result<Vec<Finding>, String> {
    let manifest = root.join("Cargo.toml");
    if !manifest.is_file() {
        return Err(String::from("Cargo.toml is missing at the root"));
    }
    let mut findings = Vec::new();
    for path in walk_files(root) {
        let rel = relative(root, &path);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let is_rust = path.extension().and_then(|ext| ext.to_str()) == Some("rs");
        if !is_rust && name != "Cargo.toml" {
            continue;
        }
        let Some(text) = read_text(&path) else {
            continue;
        };
        if is_rust {
            findings.extend(check_source(&rel, &text));
        } else {
            findings.extend(check_manifest(&rel, &text));
        }
    }
    Ok(findings)
}
