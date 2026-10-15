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
//! - the root manifest no longer sets `unsafe_code = "deny"`;
//! - a member manifest lacks `[lints] workspace = true`, which would switch
//!   the workspace lints off for that crate;
//! - a path crate in the resolved dependency graph is not a workspace member,
//!   because it cannot inherit the workspace lints at all.

use crate::util::{Finding, LineCounter, read_text, relative, walk_files};
use std::path::Path;

/// The crates that may allow `unsafe_code`, with the reason.
pub const MAY_ALLOW_UNSAFE: [(&str, &str); 9] = [
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
    (
        "bs-fsops",
        "file operations that never follow links: directory-descriptor, handle and pipe calls",
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
    let mut lines = LineCounter::default();
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
                let line = lines.line_at(bytes, index);
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

/// Does the manifest inherit the workspace lints? Cargo accepts three
/// spellings, with any spacing (tabs included) and trailing comments:
///
/// ```text
/// [lints]
/// workspace = true
///
/// lints.workspace = true          (before the first table header)
/// lints = { workspace = true }    (before the first table header)
/// ```
fn inherits_workspace_lints(text: &str) -> bool {
    let mut section: Option<String> = None;
    for line in text.lines() {
        let line: String = line
            .split('#')
            .next()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            section = Some(line);
            continue;
        }
        let inherits = match section.as_deref() {
            Some("[lints]") => line == "workspace=true",
            Some(_) => false,
            None => {
                line == "lints.workspace=true"
                    || (line.starts_with("lints={")
                        && line.ends_with('}')
                        && line["lints={".len()..line.len() - 1]
                            .split(',')
                            .any(|item| item == "workspace=true"))
            }
        };
        if inherits {
            return true;
        }
    }
    false
}

/// Checks the manifest of a workspace member (or the root manifest).
pub fn check_manifest(rel: &str, text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    if rel == "Cargo.toml" {
        let denies = text.lines().any(|line| {
            let line: String = line
                .split('#')
                .next()
                .unwrap_or("")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            line == format!("{UNSAFE_LINT}=\"deny\"")
        });
        if !denies {
            findings.push(Finding::new(
                rel,
                None,
                "the workspace must keep `unsafe_code = \"deny\"`",
            ));
        }
    } else if !inherits_workspace_lints(text) {
        findings.push(Finding::new(
            rel,
            None,
            "must contain `[lints]` with `workspace = true` (or `lints.workspace = true`), so the workspace lints apply",
        ));
    }
    findings.extend(check_unsafe_setting(rel, text));
    findings
}

/// Checks that a crate that may not allow unsafe code does not set the lint
/// in its manifest. This applies to every crate manifest, member or not.
pub fn check_unsafe_setting(rel: &str, text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    if rel != "Cargo.toml" && crate_of(rel).is_some() && !may_allow(rel) {
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

/// The manifests of the workspace members, and of the path crates in the
/// resolved dependency graph that are not members: `(members, outside)`, as
/// relative paths.
///
/// A path crate that is not a member cannot inherit the workspace lints, so it
/// would be compiled into the product with the default lints. Separate
/// workspaces that the product does not depend on (a fuzz setup, a fixture)
/// are not in the graph and do not appear.
pub fn classify_packages(
    metadata: &serde_json::Value,
    root: &Path,
) -> Result<(Vec<String>, Vec<String>), String> {
    let member_ids: Vec<&str> = metadata["workspace_members"]
        .as_array()
        .map(|ids| ids.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    let mut members = Vec::new();
    let mut outside = Vec::new();
    for package in metadata["packages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let (Some(id), Some(path)) = (package["id"].as_str(), package["manifest_path"].as_str())
        else {
            continue;
        };
        let rel = relative(root, Path::new(path));
        if member_ids.contains(&id) {
            members.push(rel);
        } else if package["source"].is_null() {
            outside.push(rel);
        }
    }
    members.sort();
    outside.sort();
    if members.is_empty() {
        return Err(String::from("cargo metadata lists no workspace members"));
    }
    Ok((members, outside))
}

fn cargo_metadata_full(root: &Path) -> Result<serde_json::Value, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let output = std::process::Command::new(cargo)
        .current_dir(root)
        // Every feature on, so a path crate that only an optional feature
        // pulls in is in the graph too.
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--all-features",
        ])
        .output()
        .map_err(|error| format!("could not run cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cargo metadata gave invalid JSON: {error}"))
}

/// Relative paths of the workspace members' manifests, from
/// `cargo metadata`.
pub fn member_manifests(root: &Path) -> Result<Vec<String>, String> {
    Ok(classify_packages(&cargo_metadata_full(root)?, root)?.0)
}

/// Findings for path crates in the dependency graph that are not workspace
/// members.
pub fn outside_member_findings(outside: &[String]) -> Vec<Finding> {
    outside
        .iter()
        .map(|rel| {
            Finding::new(
                rel.clone(),
                None,
                "is a path crate in the dependency graph but not a workspace member, so it cannot inherit the workspace lints; make it a member",
            )
        })
        .collect()
}

/// Runs the check against the repository at `root`.
pub fn run(root: &Path) -> Result<Vec<Finding>, String> {
    let (members, outside) = classify_packages(&cargo_metadata_full(root)?, root)?;
    let mut findings = run_with_members(root, &members)?;
    findings.extend(outside_member_findings(&outside));
    Ok(findings)
}

/// Like [`run`], with the member manifests (relative paths) given. Only
/// members must inherit the workspace lints: a manifest outside the
/// workspace cannot, because Cargo would reject it.
pub fn run_with_members(root: &Path, members: &[String]) -> Result<Vec<Finding>, String> {
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
        } else if rel == "Cargo.toml" || members.contains(&rel) {
            findings.extend(check_manifest(&rel, &text));
        } else {
            findings.extend(check_unsafe_setting(&rel, &text));
        }
    }
    Ok(findings)
}
