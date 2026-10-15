//! Shared helpers for the XFS reader's tests: running the reader over a
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

pub mod mem;

use bs_scan::{ScanContext, ScanEnd, ScanOptions};
use bs_scan_xfs::{Inodes, Names, XfsError, XfsOptions, XfsReport, read_filesystem};
use bs_tree::{Aggregator, NodeFlags, NodeId, NodeKind, NodeRef, Snapshot};
use std::collections::BTreeMap;

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
    pub origin: Option<NodeId>,
}

pub struct Run {
    pub snapshot: Snapshot,
    pub aggregator: Aggregator,
    pub end: ScanEnd,
    pub report: XfsReport,
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

/// Reads the file system and applies every batch to a tree.
pub fn try_read<N: Names>(
    inodes: &dyn Inodes,
    names: &N,
    options: &XfsOptions,
) -> Result<Run, XfsError> {
    let (ctx, rx) = ScanContext::channel(ScanOptions::default(), 4096);
    let (end, report) = read_filesystem(inodes, names, b"/mnt", options, &ctx)?;
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

pub fn read(fs: &mem::MemXfs) -> Run {
    let run = try_read(
        fs,
        fs,
        &XfsOptions {
            threads: Some(3),
            ..XfsOptions::default()
        },
    )
    .expect("the read works");
    assert_eq!(
        run.applied_rejected, 0,
        "no record is refused: {:?}",
        run.refusals
    );
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
