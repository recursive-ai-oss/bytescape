//! `cargo xtask helper-deps`: the elevated helper has a tight dependency
//! allowlist. Its normal-dependency closure, on every target, must be a
//! subset of `xtask/helper-deps.allow`. Its direct build dependencies (code
//! that runs while the helper is compiled, and ships nothing but what it
//! writes) must be listed, with the reason, in
//! `xtask/helper-build-deps.allow`.

use crate::util::{
    Finding, TARGETS, cargo_metadata, normalize_crate_name, package_names, parse_allow_lines,
    read_text,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The package whose closure is checked.
pub const HELPER: &str = "bytescape-helper";

/// Parses the allow file into normalized crate names.
pub fn parse_allow(text: &str) -> BTreeSet<String> {
    parse_allow_lines(text)
        .iter()
        .map(|name| normalize_crate_name(name))
        .collect()
}

/// The names of everything the helper depends on through normal (not
/// development or build) dependencies, directly or indirectly. The helper
/// itself is not included. `None` when the helper is not in the document.
pub fn helper_closure(metadata: &Value) -> Option<BTreeSet<String>> {
    let names = package_names(metadata);
    let start = names
        .iter()
        .find(|(_, name)| name.as_str() == HELPER)
        .map(|(id, _)| id.clone())?;
    let mut deps_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for node in metadata["resolve"]["nodes"].as_array()? {
        let id = node["id"].as_str()?.to_string();
        let mut normal = Vec::new();
        for dep in node["deps"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            let is_normal = dep["dep_kinds"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind["kind"].is_null()));
            if is_normal && let Some(pkg) = dep["pkg"].as_str() {
                normal.push(pkg.to_string());
            }
        }
        deps_of.insert(id, normal);
    }
    let mut seen = BTreeSet::new();
    let mut stack = vec![start.clone()];
    while let Some(id) = stack.pop() {
        for dep in deps_of.get(&id).map(Vec::as_slice).unwrap_or(&[]) {
            if seen.insert(dep.clone()) {
                stack.push(dep.clone());
            }
        }
    }
    seen.remove(&start);
    Some(
        seen.iter()
            .filter_map(|id| names.get(id))
            .map(|name| normalize_crate_name(name))
            .collect(),
    )
}

/// The names of the helper's direct build dependencies. Build dependencies run
/// while the helper is compiled and nothing of them is linked into it, so
/// they are not part of the closure above; but code that runs in the build of
/// a program that runs as administrator is reviewed all the same. `None` when
/// the helper is not in the document.
pub fn helper_build_dependencies(metadata: &Value) -> Option<BTreeSet<String>> {
    let names = package_names(metadata);
    let start = names
        .iter()
        .find(|(_, name)| name.as_str() == HELPER)
        .map(|(id, _)| id.clone())?;
    let node = metadata["resolve"]["nodes"]
        .as_array()?
        .iter()
        .find(|node| node["id"].as_str() == Some(start.as_str()))?;
    let mut found = BTreeSet::new();
    for dep in node["deps"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let is_build = dep["dep_kinds"].as_array().is_some_and(|kinds| {
            kinds
                .iter()
                .any(|kind| kind["kind"].as_str() == Some("build"))
        });
        if is_build && let Some(name) = dep["pkg"].as_str().and_then(|id| names.get(id)) {
            found.insert(normalize_crate_name(name));
        }
    }
    Some(found)
}

/// Compares the helper's direct build dependencies with their allowlist.
pub fn check_build_dependencies(
    metadata: &Value,
    target: &str,
    allow: &BTreeSet<String>,
) -> Vec<Finding> {
    let Some(found) = helper_build_dependencies(metadata) else {
        return vec![Finding::new(
            target,
            None,
            format!("package `{HELPER}` not found in the dependency graph"),
        )];
    };
    found
        .difference(allow)
        .map(|name| {
            Finding::new(
                name.clone(),
                None,
                format!(
                    "is a build dependency of {HELPER} on {target} but is not in xtask/helper-build-deps.allow"
                ),
            )
        })
        .collect()
}

/// Compares one target's closure with the allowlist.
pub fn check_metadata(metadata: &Value, target: &str, allow: &BTreeSet<String>) -> Vec<Finding> {
    let Some(closure) = helper_closure(metadata) else {
        return vec![Finding::new(
            target,
            None,
            format!("package `{HELPER}` not found in the dependency graph"),
        )];
    };
    closure
        .difference(allow)
        .map(|name| {
            Finding::new(
                name.clone(),
                None,
                format!(
                    "is a dependency of {HELPER} on {target} but is not in xtask/helper-deps.allow"
                ),
            )
        })
        .collect()
}

/// Runs the check against the repository at `root`.
pub fn run(root: &Path) -> Result<Vec<Finding>, String> {
    let allow_text = read_text(&root.join("xtask/helper-deps.allow"))
        .ok_or_else(|| String::from("xtask/helper-deps.allow is missing"))?;
    let allow = parse_allow(&allow_text);
    let build_allow_text = read_text(&root.join("xtask/helper-build-deps.allow"))
        .ok_or_else(|| String::from("xtask/helper-build-deps.allow is missing"))?;
    let build_allow = parse_allow(&build_allow_text);
    let mut findings = Vec::new();
    for target in TARGETS {
        let metadata = cargo_metadata(root, target)?;
        findings.extend(check_metadata(&metadata, target, &allow));
        findings.extend(check_build_dependencies(&metadata, target, &build_allow));
    }
    Ok(findings)
}
