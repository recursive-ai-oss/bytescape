//! Shared helpers for the btrfs reader's tests: running the reader over a
//! source and flattening what it produced into a map by path.
#![allow(dead_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

#[cfg(feature = "image-tests")]
pub mod image;
pub mod memsrc;

use bs_scan::{ScanContext, ScanEnd, ScanOptions};
use bs_scan_btrfs::{BtrfsError, BtrfsOptions, BtrfsReport, Start, TreeSource, read_filesystem};
use bs_tree::{Aggregator, NodeFlags, NodeId, NodeKind, NodeRef, Snapshot};
use std::collections::BTreeMap;

pub const START: Start = Start {
    tree: 5,
    inode: 256,
};

/// What the tree holds for one path.
#[derive(Clone, Debug)]
pub struct Info {
    pub id: NodeId,
    pub kind: NodeKind,
    /// For a directory: its own entry only.
    pub logical: u64,
    pub allocated: u64,
    pub mtime: i64,
    pub flags: NodeFlags,
    pub category: u8,
    pub failed: bool,
    pub exclusive: Option<u64>,
    pub origin: Option<NodeId>,
}

pub struct Run {
    pub snapshot: Snapshot,
    pub aggregator: Aggregator,
    pub end: ScanEnd,
    pub report: BtrfsReport,
    pub applied_rejected: u64,
    pub parked: u64,
    /// The first reasons records were refused, for the failure message.
    pub refusals: Vec<String>,
}

impl Run {
    pub fn flat(&self) -> BTreeMap<String, Info> {
        flat(&self.snapshot)
    }
}

/// Reads `src` from `start` and applies every batch to a tree.
pub fn try_read(
    src: &dyn TreeSource,
    start: Start,
    options: &BtrfsOptions,
) -> Result<Run, BtrfsError> {
    try_read_within(src, start, options, bs_scan::Boundary::root_mount())
}

/// The same, with the mounts the scan may enter named by `boundary`.
pub fn try_read_within(
    src: &dyn TreeSource,
    start: Start,
    options: &BtrfsOptions,
    boundary: bs_scan::Boundary,
) -> Result<Run, BtrfsError> {
    let (ctx, rx) = ScanContext::channel(
        ScanOptions {
            boundary,
            ..ScanOptions::default()
        },
        4096,
    );
    let (end, report) = read_filesystem(src, start, b"/mnt", options, &ctx)?;
    drop(ctx);
    let mut agg = Aggregator::new();
    let (mut rejected, mut parked) = (0, 0);
    let mut refusals = Vec::new();
    for batch in rx.try_iter() {
        let r = agg.apply(&batch);
        rejected += u64::from(r.rejected);
        parked += u64::from(r.parked);
        for (index, why) in r.first_errors.iter().take(3) {
            refusals.push(format!("record {index:?}: {why:?}"));
        }
    }
    if end == ScanEnd::Cancelled {
        agg.mark_stopped();
    }
    agg.finish();
    let snapshot = agg.snapshot();
    snapshot.verify().expect("tree invariants");
    Ok(Run {
        snapshot,
        aggregator: agg,
        end,
        report,
        applied_rejected: rejected,
        parked,
        refusals,
    })
}

pub fn read(src: &dyn TreeSource) -> Run {
    let run = try_read(src, START, &BtrfsOptions::default()).expect("the read works");
    assert_eq!(run.applied_rejected, 0, "no record is refused");
    assert_eq!(run.parked, 0, "parents go before children");
    run
}

pub fn flat(snap: &Snapshot) -> BTreeMap<String, Info> {
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
            let (mut below_logical, mut below_allocated) = (0u64, 0u64);
            if child.kind() == NodeKind::Dir {
                for g in child.children() {
                    below_logical += g.logical();
                    below_allocated += g.allocated();
                }
            }
            out.insert(
                p.clone(),
                Info {
                    id: child.id(),
                    kind: child.kind(),
                    logical: child.logical() - below_logical,
                    allocated: child.allocated() - below_allocated,
                    mtime: child.mtime(),
                    flags: child.flags(),
                    category: child.category().0,
                    failed: child.is_unreadable(),
                    exclusive: child.exclusive_allocated(),
                    origin: child.hard_link_origin(),
                },
            );
            if child.kind() == NodeKind::Dir {
                stack.push((child, p));
            }
        }
    }
    out
}
