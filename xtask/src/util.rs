//! Helpers shared by the checks: finding files, reading text, running
//! `cargo metadata`, and reporting findings.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The four release targets. Dependency checks run for each of them, so a
/// dependency that exists only on one platform is still seen.
pub const TARGETS: [&str; 4] = [
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
];

/// Directories that never hold shipped sources.
const SKIPPED_DIRS: [&str; 3] = ["target", ".git", "node_modules"];

/// One problem found by a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// File (relative to the root) or a short subject such as a crate name.
    pub subject: String,
    /// 1-based line number, when the finding belongs to a line.
    pub line: Option<usize>,
    /// What is wrong.
    pub message: String,
}

impl Finding {
    pub fn new(
        subject: impl Into<String>,
        line: Option<usize>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            subject: subject.into(),
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{}: {}", self.subject, line, self.message),
            None => write!(f, "{}: {}", self.subject, self.message),
        }
    }
}

/// The repository root: the nearest directory, starting at the current one
/// and going up, that holds `xtask/Cargo.toml`. Falls back to the directory
/// this crate was built from.
pub fn default_root() -> PathBuf {
    if let Ok(current) = std::env::current_dir() {
        for dir in current.ancestors() {
            if dir.join("xtask/Cargo.toml").is_file() {
                return dir.to_path_buf();
            }
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap_or(manifest).to_path_buf()
}

/// All files below `dir`, sorted, skipping build output and version-control
/// directories. A missing `dir` gives an empty list.
pub fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk_into(dir, &mut found);
    found.sort();
    found
}

fn walk_into(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            let name = entry.file_name();
            let skipped = SKIPPED_DIRS.iter().any(|skip| name == *skip);
            if !skipped {
                walk_into(&path, found);
            }
        } else if kind.is_file() {
            found.push(path);
        }
    }
}

/// The file's text, or `None` when it is not valid UTF-8 (a binary file).
pub fn read_text(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

/// The path relative to `root`, with forward slashes, for stable messages.
pub fn relative(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.to_string_lossy().replace('\\', "/")
}

/// Runs `cargo metadata` for one target with every feature switched on and
/// returns the parsed JSON.
pub fn cargo_metadata(root: &Path, target: &str) -> Result<serde_json::Value, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let output = Command::new(cargo)
        .current_dir(root)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--all-features",
            "--filter-platform",
            target,
        ])
        .output()
        .map_err(|error| format!("could not run cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed for {target}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cargo metadata gave invalid JSON for {target}: {error}"))
}

/// Runs `cargo tree` for one target with every feature switched on and returns
/// one line per package in the graph: `name vVERSION (...)|feature,feature`.
/// `cargo tree` resolves features for the target exactly, which
/// `cargo metadata` does not: it also counts features that only another
/// platform's dependencies turn on.
pub fn cargo_tree_features(root: &Path, target: &str) -> Result<String, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let output = Command::new(cargo)
        .current_dir(root)
        .args([
            "tree",
            "--locked",
            "--workspace",
            "--all-features",
            "--target",
            target,
            "--edges",
            "normal,build,dev",
            "--prefix",
            "none",
            "--no-dedupe",
            "--format",
            "{p}|{f}",
        ])
        .output()
        .map_err(|error| format!("could not run cargo tree: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo tree failed for {target}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parses an allow file: one entry per line, `#` starts a comment, blank
/// lines are ignored. Entries are returned trimmed, comment removed.
pub fn parse_allow_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect()
}

/// Names of the packages in a metadata document, keyed by package id.
pub fn package_names(metadata: &serde_json::Value) -> std::collections::BTreeMap<String, String> {
    let mut names = std::collections::BTreeMap::new();
    if let Some(packages) = metadata["packages"].as_array() {
        for package in packages {
            if let (Some(id), Some(name)) = (package["id"].as_str(), package["name"].as_str()) {
                names.insert(id.to_string(), name.to_string());
            }
        }
    }
    names
}

/// Crate names are compared with `-` and `_` treated as the same.
pub fn normalize_crate_name(name: &str) -> String {
    name.replace('_', "-")
}

/// Distinct sorted copy of a list of strings.
pub fn sorted_unique(items: impl IntoIterator<Item = String>) -> Vec<String> {
    items
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
