//! The comparison logic that the live differential tests rely on, run over
//! trees built by hand: it must flag every kind of difference, accept only
//! what an exceptions table names with the exact relation it states, notice
//! entries that explain nothing, and compare hard-linked files as groups.
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

use bs_tree::{Aggregator, Batch, DirKey, FileKey, Meta, NodeFlags, NodeKind, Snapshot};
use diff::{Exception, Field, compare, tree_of};

/// A file in the root: (name, logical, allocated, mtime, link key).
type F = (&'static str, u64, u64, i64, Option<u64>);

fn build(files: &[F], dirs: &[&'static str]) -> Snapshot {
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/", &Meta::default())
        .unwrap();
    for (i, d) in dirs.iter().enumerate() {
        b.dir(
            DirKey(i as u32 + 1),
            DirKey(0),
            d.as_bytes(),
            &Meta::default(),
        )
        .unwrap();
    }
    for &(name, logical, allocated, mtime, link) in files {
        let meta = Meta {
            logical,
            allocated,
            mtime,
            link: link.map(|id| FileKey {
                volume: 1,
                id: u128::from(id),
            }),
            ..Meta::default()
        };
        b.file(NodeKind::File, DirKey(0), name.as_bytes(), &meta)
            .unwrap();
    }
    b.listing_done(DirKey(0));
    for i in 0..dirs.len() {
        b.listing_done(DirKey(i as u32 + 1));
    }
    let mut agg = Aggregator::new();
    assert_eq!(agg.apply(&b).rejected, 0);
    agg.finish();
    agg.snapshot()
}

fn exception(
    name: &'static str,
    field: Field,
    path: &'static str,
    rel: fn(u64, u64) -> bool,
) -> Exception {
    Exception {
        name,
        reason: "test",
        field,
        applies: Box::new(move |p, _, _| p == path),
        relation: Box::new(rel),
    }
}

const BASE: &[F] = &[
    ("a", 100, 4096, 10, None),
    ("b", 200, 8192, 20, None),
    ("c", 0, 0, 30, None),
];

#[test]
fn identical_trees_have_no_findings() {
    let w = tree_of(&build(BASE, &["d"]));
    let r = tree_of(&build(BASE, &["d"]));
    let out = compare(&w, &r, &[]);
    assert!(out.is_clean(), "{}", out.render());
    assert_eq!(out.compared, 4);
}

#[test]
fn every_kind_of_difference_is_found() {
    let w = tree_of(&build(BASE, &["d"]));
    let cases: Vec<(Vec<F>, Vec<&'static str>, Field)> = vec![
        (
            vec![("a", 101, 4096, 10, None), BASE[1], BASE[2]],
            vec!["d"],
            Field::Logical,
        ),
        (
            vec![("a", 100, 4097, 10, None), BASE[1], BASE[2]],
            vec!["d"],
            Field::Allocated,
        ),
        (
            vec![("a", 100, 4096, 11, None), BASE[1], BASE[2]],
            vec!["d"],
            Field::Mtime,
        ),
        (vec![BASE[0], BASE[1]], vec!["d"], Field::Presence),
        (
            vec![BASE[0], BASE[1], BASE[2], ("extra", 1, 1, 1, None)],
            vec!["d"],
            Field::Presence,
        ),
        (BASE.to_vec(), vec![], Field::Presence),
    ];
    for (files, dirs, field) in cases {
        let r = tree_of(&build(&files, &dirs));
        let out = compare(&w, &r, &[]);
        assert!(
            out.unexplained.iter().any(|f| f.field == field),
            "{field:?}: {}",
            out.render()
        );
    }
    // A file in the walker's tree that is a directory in the reader's.
    let r = tree_of(&build(&[BASE[1], BASE[2]], &["d", "a"]));
    let out = compare(&w, &r, &[]);
    assert!(
        out.unexplained.iter().any(|f| f.field == Field::Kind),
        "{}",
        out.render()
    );
}

#[test]
fn an_exception_explains_exactly_what_it_states() {
    let w = tree_of(&build(BASE, &[]));
    // The reader counts less on `a`: a named exception, with its relation.
    let r = tree_of(&build(&[("a", 100, 100, 10, None), BASE[1], BASE[2]], &[]));
    let rule = || {
        exception("inline", Field::Allocated, "a", |walker, reader| {
            reader < walker
        })
    };
    let out = compare(&w, &r, &[rule()]);
    assert!(out.is_clean(), "{}", out.render());
    assert_eq!(out.explained.len(), 1);
    // The same entry does not cover another path...
    let r = tree_of(&build(&[BASE[0], ("b", 200, 100, 20, None), BASE[2]], &[]));
    let out = compare(&w, &r, &[rule()]);
    assert_eq!(out.unexplained.len(), 1);
    assert_eq!(out.unused, vec!["inline"]);
    // ...nor the wrong direction on its own path...
    let r = tree_of(&build(&[("a", 100, 5000, 10, None), BASE[1], BASE[2]], &[]));
    let out = compare(&w, &r, &[rule()]);
    assert_eq!(out.unexplained.len(), 1, "{}", out.render());
    // ...nor another field.
    let r = tree_of(&build(&[("a", 99, 4096, 10, None), BASE[1], BASE[2]], &[]));
    let out = compare(&w, &r, &[rule()]);
    assert_eq!(out.unexplained.len(), 1);
}

#[test]
fn an_exception_that_explains_nothing_is_reported() {
    let w = tree_of(&build(BASE, &[]));
    let r = tree_of(&build(BASE, &[]));
    let out = compare(
        &w,
        &r,
        &[exception("stale", Field::Allocated, "a", |_, _| true)],
    );
    assert!(out.unexplained.is_empty());
    assert_eq!(out.unused, vec!["stale"]);
    assert!(!out.is_clean());
}

#[test]
fn hard_linked_files_are_compared_as_groups() {
    // Which name carries the bytes differs between the two; the group agrees.
    let w = tree_of(&build(
        &[
            ("x1", 500, 4096, 5, Some(7)),
            ("x2", 500, 4096, 5, Some(7)),
            BASE[0],
        ],
        &[],
    ));
    let r_swapped = tree_of(&build(
        &[
            ("x2", 500, 4096, 5, Some(7)),
            ("x1", 500, 4096, 5, Some(7)),
            BASE[0],
        ],
        &[],
    ));
    let out = compare(&w, &r_swapped, &[]);
    assert!(out.is_clean(), "{}", out.render());
    // A group whose bytes differ.
    let r_less = tree_of(&build(
        &[
            ("x1", 500, 100, 5, Some(7)),
            ("x2", 500, 100, 5, Some(7)),
            BASE[0],
        ],
        &[],
    ));
    let out = compare(&w, &r_less, &[]);
    assert!(
        out.unexplained.iter().any(|f| f.field == Field::Allocated),
        "{}",
        out.render()
    );
    // A group that is not the same set of names.
    let r_three = tree_of(&build(
        &[
            ("x1", 500, 4096, 5, Some(7)),
            ("x2", 500, 4096, 5, Some(7)),
            ("x3", 500, 4096, 5, Some(7)),
            BASE[0],
        ],
        &[],
    ));
    let out = compare(&w, &r_three, &[]);
    assert!(
        out.unexplained.iter().any(|f| f.field == Field::Links),
        "{}",
        out.render()
    );
    // The flag itself is part of the comparison.
    assert!(w["x1"].flags.contains(NodeFlags::HARD_LINK));
}
