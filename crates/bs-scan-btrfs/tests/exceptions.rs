//! The table of differences between the reader and the walker that are
//! facts of the file system: each entry explains exactly what it states and
//! nothing near it.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

#[path = "common/diff.rs"]
mod diff;
#[path = "common/exceptions.rs"]
mod exceptions;

use bs_tree::{Aggregator, Batch, DirKey, Meta, NodeFlags, NodeKind, Snapshot};
use diff::{Exception, Field, compare, tree_of};

const SECTOR: u64 = 4096;
const MIB4: u64 = 4 << 20;

/// What a node of the root holds.
#[derive(Clone, Copy)]
struct N {
    kind: NodeKind,
    logical: u64,
    allocated: u64,
    mtime: i64,
    flags: NodeFlags,
}

fn file(logical: u64, allocated: u64, flags: NodeFlags) -> N {
    N {
        kind: NodeKind::File,
        logical,
        allocated,
        mtime: 10,
        flags,
    }
}

fn dir(mtime: i64) -> N {
    N {
        kind: NodeKind::Dir,
        logical: 0,
        allocated: 0,
        mtime,
        flags: NodeFlags::EMPTY,
    }
}

fn build(nodes: &[(&str, N)]) -> Snapshot {
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/", &Meta::default())
        .unwrap();
    let mut next = 1;
    for (name, n) in nodes {
        let meta = Meta {
            logical: n.logical,
            allocated: n.allocated,
            mtime: n.mtime,
            flags: n.flags,
            ..Meta::default()
        };
        if n.kind == NodeKind::Dir {
            b.dir(DirKey(next), DirKey(0), name.as_bytes(), &meta)
                .unwrap();
            b.listing_done(DirKey(next));
            next += 1;
        } else {
            b.file(n.kind, DirKey(0), name.as_bytes(), &meta).unwrap();
        }
    }
    b.listing_done(DirKey(0));
    let mut agg = Aggregator::new();
    assert_eq!(agg.apply(&b).rejected, 0);
    agg.finish();
    agg.snapshot()
}

/// Compares and returns (explained, unexplained) as (field, path) lists.
type Found = Vec<(Field, String)>;

fn judge(
    walker: &[(&str, N)],
    reader: &[(&str, N)],
    table: &[Exception],
) -> (Found, Found, Vec<&'static str>) {
    let out = compare(&tree_of(&build(walker)), &tree_of(&build(reader)), table);
    let pick = |v: &[diff::Finding]| {
        v.iter()
            .map(|f| (f.field, f.path.clone()))
            .collect::<Found>()
    };
    (
        out.explained
            .iter()
            .map(|(f, _)| (f.field, f.path.clone()))
            .collect(),
        pick(&out.unexplained),
        out.unused,
    )
}

const COMPRESSED: NodeFlags = NodeFlags::SPARSE_OR_COMPRESSED;

#[test]
fn a_compressed_file_that_the_kernel_counts_at_full_length_is_explained() {
    // text.txt of the live fixture: 4 MiB of text that compresses to 128 KiB.
    let w = [("text.txt", file(MIB4, MIB4, COMPRESSED))];
    let r = [("text.txt", file(MIB4, 131_072, COMPRESSED))];
    let (explained, unexplained, unused) = judge(&w, &r, &[exceptions::compressed_extents()]);
    assert_eq!(explained, vec![(Field::Allocated, "text.txt".to_owned())]);
    assert!(unexplained.is_empty() && unused.is_empty());
    // A length that is not a whole number of sectors is rounded up.
    let w = [("t", file(MIB4 + 1, MIB4 + SECTOR, COMPRESSED))];
    let r = [("t", file(MIB4 + 1, 8192, COMPRESSED))];
    let (explained, unexplained, _) = judge(&w, &r, &[exceptions::compressed_extents()]);
    assert_eq!(explained.len(), 1);
    assert!(unexplained.is_empty());
}

#[test]
fn nothing_near_a_compressed_file_is_explained_by_it() {
    let table = [exceptions::compressed_extents()];
    let flagged = |allocated| file(MIB4, allocated, COMPRESSED);
    let plain = |allocated| file(MIB4, allocated, NodeFlags::EMPTY);
    // The reader counts more than the walker: the wrong way round.
    let (e, u, _) = judge(
        &[("f", flagged(MIB4))],
        &[("f", flagged(MIB4 + SECTOR))],
        &table,
    );
    assert!(e.is_empty() && u.len() == 1, "{e:?} {u:?}");
    // The walker's figure is not the file's length: something else is going on.
    let (e, u, _) = judge(
        &[("f", flagged(MIB4 / 2))],
        &[("f", flagged(131_072))],
        &table,
    );
    assert!(e.is_empty() && u.len() == 1, "{e:?} {u:?}");
    let (e, u, _) = judge(
        &[("f", flagged(MIB4 + 2 * SECTOR))],
        &[("f", flagged(131_072))],
        &table,
    );
    assert!(e.is_empty() && u.len() == 1, "{e:?} {u:?}");
    // The reader did not see a compressed or sparse file.
    let (e, u, _) = judge(&[("f", plain(MIB4))], &[("f", plain(131_072))], &table);
    assert!(e.is_empty() && u.len() == 1, "{e:?} {u:?}");
    // An empty file has no whole length to match.
    let (e, u, _) = judge(
        &[("f", file(0, 0, COMPRESSED))],
        &[("f", file(0, 4096, COMPRESSED))],
        &table,
    );
    assert!(e.is_empty() && u.len() == 1, "{e:?} {u:?}");
    // Only the allocated size is covered: a different length is not.
    let (e, u, _) = judge(
        &[("f", file(MIB4, MIB4, COMPRESSED))],
        &[("f", file(MIB4 - 1, 131_072, COMPRESSED))],
        &table,
    );
    assert_eq!(e, vec![(Field::Allocated, "f".to_owned())]);
    assert!(u.iter().any(|(f, _)| *f == Field::Logical), "{u:?}");
    // An entry that explains nothing is reported.
    let (_, _, unused) = judge(&[("f", plain(1))], &[("f", plain(1))], &table);
    assert_eq!(unused, vec!["compressed extents"]);
}

#[test]
fn a_made_up_empty_directory_may_have_no_time_but_nothing_else_may() {
    let table = [exceptions::stand_in_directory_time()];
    // The walker's own visit time, against none.
    let (e, u, _) = judge(
        &[("snap1", dir(5)), ("inner", dir(1_760_000_000))],
        &[("snap1", dir(5)), ("inner", dir(0))],
        &table,
    );
    assert_eq!(e, vec![(Field::Mtime, "inner".to_owned())]);
    assert!(u.is_empty());
    // A directory with a time that is merely different is a finding.
    let (e, u, _) = judge(&[("d", dir(100))], &[("d", dir(99))], &table);
    assert!(e.is_empty() && u.len() == 1, "{e:?} {u:?}");
    // A file with no time is not a stand-in.
    let no_time = N {
        mtime: 0,
        ..file(1, 1, NodeFlags::EMPTY)
    };
    let (e, u, _) = judge(
        &[("f", file(1, 1, NodeFlags::EMPTY))],
        &[("f", no_time)],
        &table,
    );
    assert!(e.is_empty() && u.len() == 1, "{e:?} {u:?}");
    // Zero on the walker's side is no stand-in either.
    let (e, u, _) = judge(&[("d", dir(0))], &[("d", dir(7))], &table);
    assert!(e.is_empty() && u.len() == 1, "{e:?} {u:?}");
    // And it covers the time only.
    let mut with_size = dir(0);
    with_size.logical = 1;
    let (_, u, _) = judge(&[("d", dir(9))], &[("d", with_size)], &table);
    assert!(u.iter().any(|(f, _)| *f == Field::Logical), "{u:?}");
}

#[test]
fn the_whole_table_is_one_entry_for_each_reason() {
    let names: Vec<_> = exceptions::all().iter().map(|e| e.name).collect();
    assert_eq!(
        names,
        vec![
            "inline data",
            "shared extents",
            "partly used extent",
            "compressed extents",
            "stand-in directory time"
        ]
    );
    for e in exceptions::all() {
        assert!(e.reason.len() > 60, "{}: say why", e.name);
    }
}
