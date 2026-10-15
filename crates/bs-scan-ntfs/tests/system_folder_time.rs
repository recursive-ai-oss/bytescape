//! The one allowed difference in the Windows volume comparison that is about
//! the system and not the reader: the modification time of the folder
//! Windows keeps at the root of a volume. The comparison itself runs only on
//! Windows; these tests run the same table and the same check on trees made
//! in memory, to show that the exception covers that one field of that one
//! folder and nothing else.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use bs_scan::ScanResult;
use bs_scan_ntfs::NtfsOptions;
use common::compare::*;
use common::testkit::{ROOT, Spec, Volume, scan_memory};

const T0: i64 = 1_700_000_500;

/// What a volume looks like to the tests: a few folders and files, the system
/// folder at the root among them, with the times given for the system folder
/// and for `docs`.
struct Shape {
    system_folder_time: i64,
    docs_time: i64,
    /// A file inside the system folder, with this size and time.
    inside: Option<(u64, i64)>,
    /// A folder of the same name inside `docs`, with this time.
    nested_namesake: Option<i64>,
    /// Leaves the system folder out of the root.
    no_system_folder: bool,
    /// Makes the system folder a file.
    system_folder_is_a_file: bool,
}

impl Shape {
    fn plain() -> Shape {
        Shape {
            system_folder_time: T0,
            docs_time: T0,
            inside: None,
            nested_namesake: None,
            no_system_folder: false,
            system_folder_is_a_file: false,
        }
    }
}

fn scan(shape: &Shape) -> ScanResult {
    let mut v = Volume::new(Spec::default());
    let docs = v.dir("docs", ROOT);
    v.set_mtime(docs, shape.docs_time);
    v.file("readme.txt", docs, 300);
    if !shape.no_system_folder {
        let sys = if shape.system_folder_is_a_file {
            v.file(SYSTEM_FOLDER, ROOT, 10)
        } else {
            v.dir(SYSTEM_FOLDER, ROOT)
        };
        v.set_mtime(sys, shape.system_folder_time);
        if let Some((size, time)) = shape.inside {
            let f = v.file("tracking.dat", sys, size);
            v.set_mtime(f, time);
        }
    }
    if let Some(time) = shape.nested_namesake {
        let n = v.dir(SYSTEM_FOLDER, docs);
        v.set_mtime(n, time);
    }
    let (result, _) = scan_memory(v.finish(), NtfsOptions::default());
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    result
}

/// Compares `reader` with `walker` under only the system-folder exception and
/// returns (allowed, left over) as (field, path) pairs.
/// A difference as (field, path).
type Found = Vec<(String, String)>;

fn judge(reader: &Shape, walker: &Shape) -> (Found, Found) {
    let (r, w) = (scan(reader), scan(walker));
    let diffs = compare_trees(&r.snapshot, &w.snapshot);
    let (allowed, rest) =
        apply_exceptions(diffs, &[&r.snapshot, &w.snapshot], &[SYSTEM_FOLDER_TIME]);
    let pair = |d: &Diff| (d.field.to_owned(), d.path.clone());
    (
        allowed.iter().map(|(d, _)| pair(d)).collect(),
        rest.iter().map(pair).collect(),
    )
}

fn only(field: &str, path: &str) -> Found {
    vec![(field.to_owned(), path.to_owned())]
}

fn system_folder_diff(reader: i64, walker: i64) -> Diff {
    Diff {
        path: SYSTEM_FOLDER.into(),
        field: "mtime",
        reader: reader.to_string(),
        other: walker.to_string(),
    }
}

#[test]
fn a_later_time_on_the_system_folder_is_the_one_allowed_difference() {
    let reader = Shape {
        system_folder_time: T0 + 1,
        ..Shape::plain()
    };
    let (allowed, rest) = judge(&reader, &Shape::plain());
    assert_eq!(allowed, only("mtime", SYSTEM_FOLDER));
    assert!(rest.is_empty(), "{rest:?}");
}

#[test]
fn the_time_of_any_other_folder_is_not_covered() {
    let reader = Shape {
        docs_time: T0 + 1,
        ..Shape::plain()
    };
    let (allowed, rest) = judge(&reader, &Shape::plain());
    assert!(allowed.is_empty(), "{allowed:?}");
    assert_eq!(rest, only("mtime", "docs"));
}

#[test]
fn a_difference_in_both_is_split_between_them() {
    let reader = Shape {
        system_folder_time: T0 + 1,
        docs_time: T0 + 1,
        ..Shape::plain()
    };
    let (allowed, rest) = judge(&reader, &Shape::plain());
    assert_eq!(allowed, only("mtime", SYSTEM_FOLDER));
    assert_eq!(rest, only("mtime", "docs"));
}

#[test]
fn what_is_inside_the_system_folder_is_not_covered() {
    let reader = Shape {
        system_folder_time: T0 + 1,
        inside: Some((5000, T0 + 7)),
        ..Shape::plain()
    };
    let walker = Shape {
        inside: Some((9000, T0)),
        ..Shape::plain()
    };
    let (allowed, rest) = judge(&reader, &walker);
    assert_eq!(allowed, only("mtime", SYSTEM_FOLDER));
    let child = format!("{SYSTEM_FOLDER}/tracking.dat");
    let mut want = vec![
        ("allocated".to_owned(), child.clone()),
        ("logical".to_owned(), child.clone()),
        ("mtime".to_owned(), child),
    ];
    let mut got = rest;
    want.sort();
    got.sort();
    assert_eq!(got, want);
}

#[test]
fn a_system_folder_that_is_missing_on_one_side_is_not_covered() {
    let gone = Shape {
        no_system_folder: true,
        ..Shape::plain()
    };
    // (A volume with one record fewer also has a smaller table, which shows
    // as a difference of its own; only the folder's presence matters here.)
    for (reader, walker) in [(&Shape::plain(), &gone), (&gone, &Shape::plain())] {
        let (allowed, rest) = judge(reader, walker);
        assert!(allowed.is_empty(), "{allowed:?}");
        assert!(
            rest.contains(&("presence".to_owned(), SYSTEM_FOLDER.to_owned())),
            "{rest:?}"
        );
    }
}

#[test]
fn a_file_of_that_name_or_a_folder_of_that_name_elsewhere_is_not_covered() {
    // Folder on one side, file on the other: the kinds differ, which no
    // exception covers.
    let file = Shape {
        system_folder_is_a_file: true,
        ..Shape::plain()
    };
    let (allowed, rest) = judge(&Shape::plain(), &file);
    assert!(allowed.is_empty(), "{allowed:?}");
    assert_eq!(rest, only("kind", SYSTEM_FOLDER));

    // A file of that name with a different time on the two sides.
    let (a, b) = (
        Shape {
            system_folder_is_a_file: true,
            system_folder_time: T0 + 1,
            ..Shape::plain()
        },
        Shape {
            system_folder_is_a_file: true,
            ..Shape::plain()
        },
    );
    let (allowed, rest) = judge(&a, &b);
    assert!(allowed.is_empty(), "{allowed:?}");
    assert_eq!(rest, only("mtime", SYSTEM_FOLDER));

    // A folder of that name below another folder is an ordinary folder.
    let nested = Shape {
        nested_namesake: Some(T0 + 1),
        ..Shape::plain()
    };
    let base = Shape {
        nested_namesake: Some(T0),
        ..Shape::plain()
    };
    let (allowed, rest) = judge(&nested, &base);
    assert!(allowed.is_empty(), "{allowed:?}");
    assert_eq!(rest, only("mtime", &format!("docs/{SYSTEM_FOLDER}")));
}

#[test]
fn no_other_field_of_the_system_folder_is_covered() {
    let reader = Shape::plain();
    let r = scan(&reader);
    for field in [
        "presence",
        "kind",
        "flags",
        "category",
        "logical",
        "allocated",
    ] {
        let d = Diff {
            path: SYSTEM_FOLDER.into(),
            field,
            reader: "1".into(),
            other: "2".into(),
        };
        let (allowed, rest) = apply_exceptions(vec![d], &[&r.snapshot], &[SYSTEM_FOLDER_TIME]);
        assert!(allowed.is_empty(), "{field} must not be covered");
        assert_eq!(rest.len(), 1);
    }
}

#[test]
fn the_claim_holds_for_a_later_time_and_is_checked_against_the_system() {
    let d = system_folder_diff(T0 + 1, T0);
    assert!(check_system_folder_time(&d, None).is_ok());
    assert!(check_system_folder_time(&d, Some(T0 + 1)).is_ok());
    assert!(check_system_folder_time(&d, Some(T0 + 60)).is_ok());
}

#[test]
fn the_claim_fails_for_an_earlier_or_equal_time_or_one_the_system_denies() {
    // The reader older than the walker: no write explains that.
    let older = system_folder_diff(T0 - 1, T0);
    assert!(check_system_folder_time(&older, None).is_err());
    let equal = system_folder_diff(T0, T0);
    assert!(check_system_folder_time(&equal, Some(T0)).is_err());
    // The reader reports a time the system itself does not report later.
    let later = system_folder_diff(T0 + 5, T0);
    assert!(check_system_folder_time(&later, Some(T0 + 4)).is_err());
    // Not a number.
    let garbage = Diff {
        reader: "soon".into(),
        ..system_folder_diff(T0 + 1, T0)
    };
    assert!(check_system_folder_time(&garbage, None).is_err());
}

#[test]
fn the_claim_is_only_made_for_that_field_of_that_folder() {
    let other_path = Diff {
        path: "docs".into(),
        ..system_folder_diff(T0 + 1, T0)
    };
    assert!(check_system_folder_time(&other_path, None).is_err());
    let other_field = Diff {
        field: "logical",
        ..system_folder_diff(T0 + 1, T0)
    };
    assert!(check_system_folder_time(&other_field, None).is_err());
}
