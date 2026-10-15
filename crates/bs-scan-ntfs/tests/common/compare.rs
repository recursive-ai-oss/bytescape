//! Comparing two trees of the same volume, path by path, with an explicit
//! table of the differences that are allowed and why.
#![allow(dead_code)]

use super::testkit::{Info, flatten};
use bs_scan::ScanResult;
use bs_tree::{NodeFlags, NodeId, Snapshot};
use std::collections::BTreeMap;

/// A difference between two trees at one path.
#[derive(Clone, Debug)]
pub struct Diff {
    pub path: String,
    pub field: &'static str,
    pub reader: String,
    pub other: String,
}

/// Why a difference is allowed. Every exception names the field it covers
/// and gives its reason; a difference without one fails the test.
pub struct Exception {
    pub field: &'static str,
    pub reason: &'static str,
    /// Whether the exception covers this path.
    pub covers: fn(&str, &Info) -> bool,
}

pub fn lossy(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

/// The flags the comparison looks at.
fn relevant() -> NodeFlags {
    NodeFlags::HARD_LINK
        | NodeFlags::SPARSE_OR_COMPRESSED
        | NodeFlags::NOT_FOLLOWED
        | NodeFlags::UNREADABLE
        | NodeFlags::CLOUD_PLACEHOLDER
}

/// Compares the reader's tree with another tree (the walker's) path by path.
/// Hard-linked names are compared as groups, because which name carries the
/// bytes depends on the order names are met in.
pub fn compare_trees(reader: &Snapshot, other: &Snapshot) -> Vec<Diff> {
    let a = flatten(reader);
    let b = flatten(other);
    let mut diffs = Vec::new();
    let mut push = |p: &[u8], field: &'static str, r: String, o: String| {
        diffs.push(Diff {
            path: lossy(p),
            field,
            reader: r,
            other: o,
        });
    };
    for (path, x) in &a {
        let Some(y) = b.get(path) else {
            push(path, "presence", "present".into(), "missing".into());
            continue;
        };
        if x.kind != y.kind {
            push(
                path,
                "kind",
                format!("{:?}", x.kind),
                format!("{:?}", y.kind),
            );
            continue;
        }
        let fx = x.flags.bits() & relevant().bits();
        let fy = y.flags.bits() & relevant().bits();
        if fx != fy {
            push(path, "flags", format!("{fx:#x}"), format!("{fy:#x}"));
        }
        if x.mtime != y.mtime {
            push(path, "mtime", x.mtime.to_string(), y.mtime.to_string());
        }
        if x.category != y.category {
            push(
                path,
                "category",
                x.category.to_string(),
                y.category.to_string(),
            );
        }
        if !x.flags.contains(NodeFlags::HARD_LINK) {
            if x.logical != y.logical {
                push(
                    path,
                    "logical",
                    x.logical.to_string(),
                    y.logical.to_string(),
                );
            }
            if x.allocated != y.allocated {
                push(
                    path,
                    "allocated",
                    x.allocated.to_string(),
                    y.allocated.to_string(),
                );
            }
        }
    }
    for path in b.keys() {
        if !a.contains_key(path) {
            push(path, "presence", "missing".into(), "present".into());
        }
    }
    // Hard-link groups: the same names, the same total bytes.
    let groups = |m: &BTreeMap<Vec<u8>, Info>| {
        let mut members: BTreeMap<NodeId, Vec<Vec<u8>>> = BTreeMap::new();
        let mut bytes: BTreeMap<NodeId, (u64, u64)> = BTreeMap::new();
        for (p, i) in m {
            if !i.flags.contains(NodeFlags::HARD_LINK) {
                continue;
            }
            let head = i.origin.unwrap_or(i.id);
            members.entry(head).or_default().push(p.clone());
            let e = bytes.entry(head).or_default();
            e.0 += i.logical;
            e.1 += i.allocated;
        }
        let mut groups: BTreeMap<Vec<Vec<u8>>, (u64, u64)> = BTreeMap::new();
        for (head, mut names) in members {
            names.sort();
            groups.insert(names, bytes[&head]);
        }
        groups
    };
    let (ga, gb) = (groups(&a), groups(&b));
    for (names, bytes) in &ga {
        let label = lossy(names.first().map_or(&[][..], |n| n));
        match gb.get(names) {
            None => diffs.push(Diff {
                path: label,
                field: "link group",
                reader: format!("{} names", names.len()),
                other: "no such group".into(),
            }),
            Some(o) if o != bytes => diffs.push(Diff {
                path: label,
                field: "link group bytes",
                reader: format!("{bytes:?}"),
                other: format!("{o:?}"),
            }),
            _ => {}
        }
    }
    for names in gb.keys() {
        if !ga.contains_key(names) {
            diffs.push(Diff {
                path: lossy(names.first().map_or(&[][..], |n| n)),
                field: "link group",
                reader: "no such group".into(),
                other: format!("{} names", names.len()),
            });
        }
    }
    diffs
}

/// Splits differences into those an exception covers (with its reason) and
/// those it does not. The path is looked up in the reader's tree, then in
/// the other tree, so an exception can cover entries only one side has.
pub fn apply_exceptions(
    diffs: Vec<Diff>,
    trees: &[&Snapshot],
    table: &[Exception],
) -> (Vec<(Diff, &'static str)>, Vec<Diff>) {
    let infos: Vec<BTreeMap<Vec<u8>, Info>> = trees.iter().map(|t| flatten(t)).collect();
    let mut allowed = Vec::new();
    let mut rest = Vec::new();
    'next: for d in diffs {
        let key = d.path.as_bytes().to_vec();
        for map in &infos {
            if let Some(info) = map.get(&key) {
                for e in table {
                    if e.field == d.field && (e.covers)(&d.path, info) {
                        allowed.push((d, e.reason));
                        continue 'next;
                    }
                }
            }
        }
        rest.push(d);
    }
    (allowed, rest)
}

/// Fails the test with every uncovered difference listed.
pub fn assert_no_diffs(what: &str, diffs: &[Diff]) {
    if diffs.is_empty() {
        return;
    }
    let mut text = format!("{what}: {} differences\n", diffs.len());
    for d in diffs.iter().take(60) {
        text.push_str(&format!(
            "  {:?} {}: reader {} / other {}\n",
            d.path, d.field, d.reader, d.other
        ));
    }
    panic!("{text}");
}

/// Checks the invariants of any finished scan.
pub fn assert_sound(result: &ScanResult) {
    assert!(
        result.stats.error.is_none(),
        "scan failed: {:?}",
        result.stats.error
    );
    assert_eq!(result.stats.records_parked, 0, "parents go before children");
    assert_eq!(result.stats.records_rejected, 0);
    result.snapshot.verify().expect("tree invariants");
}
