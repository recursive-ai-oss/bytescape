//! Comparing what a fast reader produced with what the directory walker
//! produced for the same file system, path by path.
//!
//! The comparison is exact. Where the two legitimately differ, the difference
//! must be named in an exceptions table: each entry says which paths, which
//! field, why, and the exact relation that must hold between the two values
//! (never a tolerance). A difference that no entry explains is a finding. An
//! entry that explains nothing is reported too, so the table cannot grow
//! stale.
//!
//! Hard-linked files are compared as groups: which name of a group carries
//! the bytes depends on the order a producer meets the names in, so the
//! numbers are summed over the group.
#![allow(dead_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use bs_tree::{NodeFlags, NodeId, NodeKind, NodeRef, Snapshot};
use std::collections::{BTreeMap, BTreeSet};

/// What a tree holds for one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub kind: NodeKind,
    /// For a directory: its own entry only.
    pub logical: u64,
    pub allocated: u64,
    pub mtime: i64,
    pub flags: NodeFlags,
    pub failed: bool,
    pub id: NodeId,
    pub origin: Option<NodeId>,
}

pub type Tree = BTreeMap<String, Node>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Field {
    Presence,
    Kind,
    Logical,
    Allocated,
    Mtime,
    Links,
    Failed,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub path: String,
    pub field: Field,
    pub walker: String,
    pub reader: String,
}

/// Whether an exception covers a path: its path, the walker's node and the
/// reader's.
pub type Applies = Box<dyn Fn(&str, &Node, &Node) -> bool>;

/// One documented difference.
pub struct Exception {
    /// A name for the entry, for the report.
    pub name: &'static str,
    /// Why the two differ.
    pub reason: &'static str,
    pub field: Field,
    /// Whether the entry applies to this path (the fixture's paths, or a
    /// property of the file).
    pub applies: Applies,
    /// The exact relation that must hold between the walker's value and the
    /// reader's, as numbers (for fields that are numbers).
    pub relation: Box<dyn Fn(u64, u64) -> bool>,
}

pub struct Outcome {
    pub unexplained: Vec<Finding>,
    pub explained: Vec<(Finding, &'static str)>,
    pub unused: Vec<&'static str>,
    pub compared: usize,
}

impl Outcome {
    pub fn render(&self) -> String {
        let mut out = format!(
            "{} paths compared, {} differences explained, {} unexplained\n",
            self.compared,
            self.explained.len(),
            self.unexplained.len()
        );
        for f in &self.unexplained {
            out.push_str(&format!(
                "UNEXPLAINED {:?} {}: walker {} reader {}\n",
                f.field, f.path, f.walker, f.reader
            ));
        }
        for (f, name) in &self.explained {
            out.push_str(&format!(
                "explained by [{name}] {:?} {}: walker {} reader {}\n",
                f.field, f.path, f.walker, f.reader
            ));
        }
        for u in &self.unused {
            out.push_str(&format!("UNUSED exception [{u}]\n"));
        }
        out
    }

    pub fn is_clean(&self) -> bool {
        self.unexplained.is_empty() && self.unused.is_empty()
    }
}

fn node_of(n: NodeRef<'_>) -> Node {
    let mut logical = n.logical();
    let mut allocated = n.allocated();
    if n.kind() == NodeKind::Dir {
        for c in n.children() {
            logical -= c.logical();
            allocated -= c.allocated();
        }
    }
    Node {
        kind: n.kind(),
        logical,
        allocated,
        mtime: n.mtime(),
        flags: n.flags(),
        failed: n.is_unreadable(),
        id: n.id(),
        origin: n.hard_link_origin(),
    }
}

/// Every node below the root, by path relative to it.
pub fn tree_of(snap: &Snapshot) -> Tree {
    let mut out = BTreeMap::new();
    let Some(root) = snap.root() else {
        return out;
    };
    let mut stack: Vec<(NodeRef<'_>, String)> = vec![(root, String::new())];
    while let Some((node, path)) = stack.pop() {
        for child in node.children() {
            let mut p = path.clone();
            if !p.is_empty() {
                p.push('/');
            }
            p.push_str(&String::from_utf8_lossy(child.name()));
            out.insert(p.clone(), node_of(child));
            if child.kind() == NodeKind::Dir {
                stack.push((child, p));
            }
        }
    }
    out
}

/// The hard-link groups of a tree: sets of paths that are names of one file.
fn link_groups(t: &Tree) -> BTreeSet<BTreeSet<String>> {
    let mut by_origin: BTreeMap<NodeId, BTreeSet<String>> = BTreeMap::new();
    for (path, n) in t {
        if n.flags.contains(NodeFlags::HARD_LINK) {
            let key = n.origin.unwrap_or(n.id);
            by_origin.entry(key).or_default().insert(path.clone());
        }
    }
    by_origin.into_values().collect()
}

pub fn compare(walker: &Tree, reader: &Tree, exceptions: &[Exception]) -> Outcome {
    let mut out = Outcome {
        unexplained: Vec::new(),
        explained: Vec::new(),
        unused: Vec::new(),
        compared: 0,
    };
    let mut used = vec![false; exceptions.len()];
    let paths: BTreeSet<&String> = walker.keys().chain(reader.keys()).collect();
    let finding = |out: &mut Outcome,
                   used: &mut Vec<bool>,
                   f: Finding,
                   numbers: Option<(u64, u64)>,
                   w: Option<&Node>,
                   r: Option<&Node>| {
        if let (Some(w), Some(r)) = (w, r) {
            for (i, e) in exceptions.iter().enumerate() {
                if e.field == f.field
                    && (e.applies)(&f.path, w, r)
                    && numbers.is_none_or(|(a, b)| (e.relation)(a, b))
                {
                    used[i] = true;
                    out.explained.push((f, e.name));
                    return;
                }
            }
        } else {
            // Presence: the exception sees whichever side exists.
            let any = w.or(r).unwrap();
            for (i, e) in exceptions.iter().enumerate() {
                if e.field == Field::Presence && (e.applies)(&f.path, any, any) {
                    used[i] = true;
                    out.explained.push((f, e.name));
                    return;
                }
            }
        }
        out.unexplained.push(f);
    };
    // Hard-linked paths: numbers are compared per group below.
    let linked = |n: &Node| n.flags.contains(NodeFlags::HARD_LINK);
    for path in paths {
        out.compared += 1;
        let (w, r) = (walker.get(path), reader.get(path));
        let (Some(wn), Some(rn)) = (w, r) else {
            finding(
                &mut out,
                &mut used,
                Finding {
                    path: path.clone(),
                    field: Field::Presence,
                    walker: format!("{}", w.is_some()),
                    reader: format!("{}", r.is_some()),
                },
                None,
                w,
                r,
            );
            continue;
        };
        let check = |out: &mut Outcome, used: &mut Vec<bool>, field, a: String, b: String, nums| {
            finding(
                out,
                used,
                Finding {
                    path: path.clone(),
                    field,
                    walker: a,
                    reader: b,
                },
                nums,
                Some(wn),
                Some(rn),
            );
        };
        if wn.kind != rn.kind {
            check(
                &mut out,
                &mut used,
                Field::Kind,
                format!("{:?}", wn.kind),
                format!("{:?}", rn.kind),
                None,
            );
            continue;
        }
        if wn.failed != rn.failed {
            check(
                &mut out,
                &mut used,
                Field::Failed,
                wn.failed.to_string(),
                rn.failed.to_string(),
                None,
            );
        }
        if !(linked(wn) || linked(rn)) {
            if wn.logical != rn.logical {
                check(
                    &mut out,
                    &mut used,
                    Field::Logical,
                    wn.logical.to_string(),
                    rn.logical.to_string(),
                    Some((wn.logical, rn.logical)),
                );
            }
            if wn.allocated != rn.allocated {
                check(
                    &mut out,
                    &mut used,
                    Field::Allocated,
                    wn.allocated.to_string(),
                    rn.allocated.to_string(),
                    Some((wn.allocated, rn.allocated)),
                );
            }
        }
        if wn.mtime != rn.mtime {
            check(
                &mut out,
                &mut used,
                Field::Mtime,
                wn.mtime.to_string(),
                rn.mtime.to_string(),
                Some((wn.mtime as u64, rn.mtime as u64)),
            );
        }
    }
    // Link groups: the same names, and the same sums.
    let (wg, rg) = (link_groups(walker), link_groups(reader));
    for g in wg.symmetric_difference(&rg) {
        out.unexplained.push(Finding {
            path: g.iter().cloned().collect::<Vec<_>>().join(" = "),
            field: Field::Links,
            walker: wg.contains(g).to_string(),
            reader: rg.contains(g).to_string(),
        });
    }
    for g in wg.intersection(&rg) {
        let sum = |t: &Tree, f: fn(&Node) -> u64| g.iter().map(|p| f(&t[p])).sum::<u64>();
        for (field, f) in [
            (Field::Logical, (|n: &Node| n.logical) as fn(&Node) -> u64),
            (Field::Allocated, |n: &Node| n.allocated),
        ] {
            let (a, b) = (sum(walker, f), sum(reader, f));
            if a != b {
                let first = g.iter().next().unwrap();
                let (w, r) = (&walker[first], &reader[first]);
                let fnd = Finding {
                    path: g.iter().cloned().collect::<Vec<_>>().join(" = "),
                    field,
                    walker: a.to_string(),
                    reader: b.to_string(),
                };
                let mut explained = None;
                for (i, e) in exceptions.iter().enumerate() {
                    if e.field == field && (e.applies)(first, w, r) && (e.relation)(a, b) {
                        used[i] = true;
                        explained = Some(e.name);
                        break;
                    }
                }
                match explained {
                    Some(name) => out.explained.push((fnd, name)),
                    None => out.unexplained.push(fnd),
                }
            }
        }
    }
    for (i, e) in exceptions.iter().enumerate() {
        if !used[i] {
            out.unused.push(e.name);
        }
    }
    out
}
