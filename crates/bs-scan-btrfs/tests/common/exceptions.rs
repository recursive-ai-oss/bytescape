//! The differences between the btrfs reader and the directory walker that
//! are facts of the file system and not faults of either. Each entry names
//! the field, the paths it covers, the exact relation between the two
//! figures, and where the behaviour is documented. The live comparison and
//! the tests of this table share it.
//!
//! A new difference is a finding. It does not go in this table until its
//! cause is understood and written down here with its source.
#![allow(dead_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use crate::diff::{Exception, Field, Node, Outcome, Tree};
use bs_tree::{NodeFlags, NodeKind};
use std::collections::BTreeMap;

/// The sector size the kernel rounds `st_blocks` to on the file systems the
/// tests make (4 KiB).
const SECTOR: u64 = 4096;

pub fn inline_data() -> Exception {
    Exception {
        name: "inline data",
        reason: "A file whose data is kept inside the tree's own blocks is reported by the \
                 kernel (st_blocks) as one whole sector; the reader counts the bytes stored. \
                 The walker's figure is the reader's rounded up to a sector.",
        field: Field::Allocated,
        applies: Box::new(|_, _, r: &Node| matches!(r.kind, NodeKind::File | NodeKind::Symlink)),
        relation: Box::new(|walker, reader| reader > 0 && reader < SECTOR && walker == SECTOR),
    }
}

pub fn shared_extents() -> Exception {
    Exception {
        name: "shared extents",
        reason: "An extent used by several files is counted once, at the file the reader emits \
                 first; the walker counts it at every file (as `du` does). The reader's \
                 figure is at most the walker's.",
        field: Field::Allocated,
        applies: Box::new(|_, _, r| r.flags.contains(NodeFlags::SHARED)),
        relation: Box::new(|walker, reader| reader < walker),
    }
}

pub fn partly_used_extent() -> Exception {
    Exception {
        name: "partly used extent",
        reason: "overwritten.bin has a 4 KiB hole punched into the middle of its extent by an \
                 overwrite. The extent stays on disk whole; the reader counts it whole, the \
                 kernel counts the bytes the file refers to.",
        field: Field::Allocated,
        applies: Box::new(|p, _, _| p == "overwritten.bin"),
        relation: Box::new(|walker, reader| reader > walker),
    }
}

/// Whether `walker` is `logical` rounded up to a whole sector: the figure
/// the kernel gives for a file whose every extent counts at its full length.
fn is_whole_length(walker: u64, logical: u64) -> bool {
    logical > 0 && walker == logical.div_ceil(SECTOR).saturating_mul(SECTOR)
}

pub fn compressed_extents() -> Exception {
    Exception {
        name: "compressed extents",
        reason: "btrfs adds the length of a file's extents to the inode's byte count, which \
                 stat reports as st_blocks, from the file extent item's num_bytes: the \
                 length in the file, compressed or not (insert_reserved_file_extent and \
                 btrfs_update_inode_bytes, fs/btrfs/inode.c). So a compressed file shows its \
                 uncompressed size, as `du` does on btrfs, while its extents on disk are \
                 smaller. The reader counts what the extents take on disk, which is what \
                 deleting the file frees. The walker's figure is the file's length rounded up \
                 to a sector; the reader's is smaller.",
        field: Field::Allocated,
        applies: Box::new(|_, w, r| {
            r.kind == NodeKind::File
                && r.flags.contains(NodeFlags::SPARSE_OR_COMPRESSED)
                && is_whole_length(w.allocated, w.logical)
        }),
        relation: Box::new(|walker, reader| reader < walker),
    }
}

pub fn stand_in_directory_time() -> Exception {
    Exception {
        name: "stand-in directory time",
        reason: "Where a snapshot keeps the entry of a subvolume that is not nested in it, the \
                 kernel makes up an empty directory when the name is looked up (new_simple_dir \
                 in fs/btrfs/inode.c) and stamps it with that moment, so the walker reports the \
                 time of its own visit. Nothing on disk holds a time for it; the reader reports \
                 none (zero).",
        field: Field::Mtime,
        applies: Box::new(|_, w, r| {
            r.kind == NodeKind::Dir && w.kind == NodeKind::Dir && r.mtime == 0 && w.mtime != 0
        }),
        relation: Box::new(|walker, reader| reader == 0 && walker > 0),
    }
}

/// Everything the full live comparison may need.
pub fn all() -> Vec<Exception> {
    vec![
        inline_data(),
        shared_extents(),
        partly_used_extent(),
        compressed_extents(),
        stand_in_directory_time(),
    ]
}

/// The fixture's subvolumes and snapshots: the roots of other trees.
pub const SUBVOLUME_ROOTS: &[&str] = &["sub1", "sub1/inner", "snap1", "snap1-ro"];
/// Where the snapshots keep the entry of `sub1/inner` without nesting it.
pub const STAND_INS: &[&str] = &["snap1/inner", "snap1-ro/inner"];

/// The kind of thing a path is, for the report of what was compared.
fn class_of(path: &str, w: &Tree, r: &Tree) -> &'static str {
    if SUBVOLUME_ROOTS.contains(&path) {
        return "subvolume or snapshot root";
    }
    if STAND_INS.contains(&path) {
        return "stand-in folder (snapshot of a nested subvolume)";
    }
    let Some(n) = r.get(path).or_else(|| w.get(path)) else {
        return "unknown";
    };
    match n.kind {
        NodeKind::Dir => "folder",
        NodeKind::Symlink => "symbolic link",
        NodeKind::File => {
            if n.flags.contains(NodeFlags::HARD_LINK) {
                "hard-linked file"
            } else if n.flags.contains(NodeFlags::SHARED) {
                "file sharing extents"
            } else if n.flags.contains(NodeFlags::SPARSE_OR_COMPRESSED) {
                "compressed or sparse file"
            } else if n.allocated > 0 && n.allocated < 4096 {
                "file with inline data"
            } else {
                "plain file"
            }
        }
        _ => "special file",
    }
}

/// What was compared, by class of path: how many paths, how many differences
/// were explained by the table, how many were not. One run of the live test
/// is enough to judge from this.
pub fn class_table(w: &Tree, r: &Tree, outcome: &Outcome) -> String {
    let mut rows: BTreeMap<&'static str, (usize, usize, usize)> = BTreeMap::new();
    for path in w
        .keys()
        .chain(r.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        rows.entry(class_of(path, w, r)).or_default().0 += 1;
    }
    let first = |p: &str| p.split(" = ").next().unwrap_or(p).to_owned();
    for (f, _) in &outcome.explained {
        rows.entry(class_of(&first(&f.path), w, r)).or_default().1 += 1;
    }
    for f in &outcome.unexplained {
        rows.entry(class_of(&first(&f.path), w, r)).or_default().2 += 1;
    }
    let mut out = String::from("compared by class of path:\n");
    out.push_str(&format!(
        "  {:<50} {:>8} {:>10} {:>12}\n",
        "class", "paths", "explained", "unexplained"
    ));
    for (class, (paths, explained, unexplained)) in rows {
        out.push_str(&format!(
            "  {class:<50} {paths:>8} {explained:>10} {unexplained:>12}\n"
        ));
    }
    out
}
