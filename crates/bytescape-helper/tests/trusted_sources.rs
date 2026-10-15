//! The helper decides what is an operating system folder from the system's own
//! answers, and not from the environment it was started with: a program can
//! start it with any environment it likes.
//!
//! The Windows entry point is read on every system, so the check runs where
//! the code cannot: it fails on any use of the process environment in the
//! helper's Windows code outside its tests. The Windows tests prove the
//! behaviour itself.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

/// The code of a source file before its tests, without comment lines.
fn code_before_tests(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(file);
    let text = std::fs::read_to_string(&path).unwrap();
    let end = text.find("#[cfg(test)]").unwrap_or(text.len());
    text[..end]
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ways to read the process environment.
const ENVIRONMENT: &[&str] = &[
    "env::var",
    "env::vars",
    "var_os",
    "from_lookup",
    "env!(",
    "option_env!(",
    "ExpandEnvironmentStrings",
    "GetEnvironmentVariable",
    "GetEnvironmentStrings",
    "SHGetKnownFolderPath",
    "SHGetFolderPath",
];

#[test]
fn the_windows_entry_point_never_reads_the_process_environment() {
    for file in ["windows.rs"] {
        let code = code_before_tests(file);
        assert!(code.contains("system_knowledge"), "{file} changed shape");
        for needle in ENVIRONMENT {
            assert!(
                !code.contains(needle),
                "{file} uses `{needle}`: take the value from the system instead"
            );
        }
    }
}

#[test]
fn the_shared_folder_rules_do_not_read_the_process_environment_either() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../bs-fsops/src/folders.rs");
    let text = std::fs::read_to_string(path).unwrap();
    let end = text.find("#[cfg(test)]").unwrap();
    let code: String = text[..end]
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for needle in ENVIRONMENT {
        assert!(!code.contains(needle), "folders.rs uses `{needle}`");
    }
}

#[test]
fn the_check_would_notice_a_use() {
    // The scan is only worth anything if it can fail.
    let sample = "let root = std::env::var_os(\"SystemRoot\");";
    assert!(ENVIRONMENT.iter().any(|needle| sample.contains(needle)));
    let sample = "let env = Environment::from_lookup(|n| lookup(n));";
    assert!(ENVIRONMENT.iter().any(|needle| sample.contains(needle)));
}
