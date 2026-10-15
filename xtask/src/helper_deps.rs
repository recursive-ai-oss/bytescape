//! `cargo xtask helper-deps`: the elevated helper has a tight dependency
//! allowlist. Its normal-dependency closure, on every target, must be a
//! subset of `xtask/helper-deps.allow`. Its build-time closure (the build
//! dependencies of the helper and of every crate in its normal closure, and
//! everything those depend on: code that runs while the helper is compiled and
//! ships nothing but what it writes) must be a subset of the pinned list in
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

/// The names of everything that runs while the helper is compiled, other than
/// the helper's own normal closure: the build dependencies of the helper and of
/// every crate in its normal closure (the helper's own crates, `libc`,
/// `windows-sys`, and so on), and everything those depend on through normal or
/// build edges (development edges never count). All of it runs on the machine
/// that builds a program that runs as administrator, and nothing of it is
/// linked into that program, so it is not part of the closure above but it is
/// reviewed all the same. `None` when the helper is not in the document.
pub fn helper_build_closure(metadata: &Value) -> Option<BTreeSet<String>> {
    let names = package_names(metadata);
    let start = names
        .iter()
        .find(|(_, name)| name.as_str() == HELPER)
        .map(|(id, _)| id.clone())?;
    // Per package: its normal dependencies and its build dependencies.
    let mut normal_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut build_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for node in metadata["resolve"]["nodes"].as_array()? {
        let id = node["id"].as_str()?.to_string();
        let mut normal = Vec::new();
        let mut build = Vec::new();
        for dep in node["deps"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            let kinds = dep["dep_kinds"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let Some(pkg) = dep["pkg"].as_str() else {
                continue;
            };
            if kinds.iter().any(|kind| kind["kind"].is_null()) {
                normal.push(pkg.to_string());
            }
            if kinds
                .iter()
                .any(|kind| kind["kind"].as_str() == Some("build"))
            {
                build.push(pkg.to_string());
            }
        }
        normal_of.insert(id.clone(), normal);
        build_of.insert(id, build);
    }
    // The helper and its normal closure: the crates linked into the program.
    let mut linked: BTreeSet<String> = BTreeSet::new();
    linked.insert(start.clone());
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        for dep in normal_of.get(&id).map(Vec::as_slice).unwrap_or(&[]) {
            if linked.insert(dep.clone()) {
                stack.push(dep.clone());
            }
        }
    }
    // Seeds: the build dependencies of every linked crate. From there, both
    // kinds of edge continue the walk.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    for id in &linked {
        for dep in build_of.get(id).map(Vec::as_slice).unwrap_or(&[]) {
            if seen.insert(dep.clone()) {
                stack.push(dep.clone());
            }
        }
    }
    while let Some(id) = stack.pop() {
        let edges = normal_of
            .get(&id)
            .into_iter()
            .chain(build_of.get(&id))
            .flatten();
        for dep in edges {
            if seen.insert(dep.clone()) {
                stack.push(dep.clone());
            }
        }
    }
    // A crate that is also linked into the program is covered by the other
    // list; it only needs a name here when something reaches it at build time
    // from outside that closure, which the walk above has already decided.
    Some(
        seen.iter()
            .filter_map(|id| names.get(id))
            .map(|name| normalize_crate_name(name))
            .collect(),
    )
}

/// Compares the helper's build-time closure with its pinned allowlist.
pub fn check_build_dependencies(
    metadata: &Value,
    target: &str,
    allow: &BTreeSet<String>,
) -> Vec<Finding> {
    let Some(found) = helper_build_closure(metadata) else {
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
                    "is in the build-time closure of {HELPER} on {target} but is not in xtask/helper-build-deps.allow"
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
