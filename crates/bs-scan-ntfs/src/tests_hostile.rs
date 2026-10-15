//! Tests of the limits the reader puts on hostile input: the size of an
//! attribute list it will read from clusters, the total it will read, how
//! many extension records the table's own record may name, how deep it
//! follows directories, how many names a file may have, and the table's runs.
//! Each limit has a case just over it, built so that a reader without the
//! limit would succeed where this one refuses, which is what makes the test
//! able to fail.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use crate::emit::MAX_DEPTH;
use crate::mft::{LIST_BUDGET_BYTES, MAX_LIST_BYTES, MAX_MFT_EXTENSIONS};
use crate::parse::{MAX_NAMES_PER_FILE, drop_dos_aliases};
use crate::testkit::{
    NonRes, ROOT, RecordBuilder, SourceScanner, Spec, Volume, encode_runs, flat, nonresident_attr,
    scan_memory,
};
use crate::{NtfsOptions, NtfsReport, ProblemKind};
use bs_scan::{ScanOptions, start};
use std::time::{Duration, Instant};

const CB: u64 = 4096;

/// A stored attribute list of `size` bytes whose runs cover `clusters`
/// clusters from `lcn`.
fn stored_list(size: u64, lcn: u64, clusters: u64) -> Vec<u8> {
    nonresident_attr(
        7,
        &NonRes {
            ty: 0x20,
            name: String::new(),
            flags: 0,
            lowest_vcn: 0,
            highest_vcn: clusters - 1,
            unit: 0,
            allocated: clusters * CB,
            size,
            initialized: size,
            compressed: None,
            runs: encode_runs(&[(Some(lcn), clusters)]),
        },
    )
}

/// A volume big enough to hold lists of tens of megabytes (zeros: an
/// attribute list that reads as zeros ends at once and is valid).
fn roomy() -> Spec {
    Spec {
        clusters: 20_000,
        ..Spec::default()
    }
}

/// The error text of a read that is refused, or `None` when it succeeds.
fn refused(img: Vec<u8>, options: NtfsOptions) -> Option<String> {
    let scanner = SourceScanner::new(img, options);
    let result = start(scanner.clone(), "C:\\".into(), ScanOptions::default()).wait();
    let outcome = scanner.outcome.lock().unwrap().take();
    match outcome {
        Some(Err(text)) => Some(text),
        Some(Ok(_)) => {
            assert!(result.stats.error.is_none());
            None
        }
        None => Some(format!("no outcome: {:?}", result.stats.error)),
    }
}

fn read_ok(img: Vec<u8>) -> (bs_scan::ScanResult, NtfsReport) {
    let (result, report) = scan_memory(
        img,
        NtfsOptions {
            threads: Some(1),
            ..NtfsOptions::default()
        },
    );
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    result.snapshot.verify().unwrap();
    (result, report.unwrap())
}

/// Replaces record 0 of an image made by the kit with one that has `extra`
/// attributes besides the table's own data attribute, on the same run.
fn replace_table_record(
    img: &mut [u8],
    rb: usize,
    table_clusters: u64,
    size: u64,
    extra: Vec<Vec<u8>>,
) {
    let mut rec = RecordBuilder::new(rb, 1)
        .number(0)
        .standard_info(0x6, 1_700_000_000)
        .file_name(5, 5, 3, "$MFT")
        .nonres(&NonRes {
            ty: 0x80,
            name: String::new(),
            flags: 0,
            lowest_vcn: 0,
            highest_vcn: table_clusters - 1,
            unit: 0,
            allocated: table_clusters * CB,
            size,
            initialized: size,
            compressed: None,
            runs: encode_runs(&[(Some(4), table_clusters)]),
        });
    for a in extra {
        rec = rec.raw(a);
    }
    let at = (4 * CB) as usize;
    img[at..at + rb].copy_from_slice(&rec.build());
}

/// The image of a small volume (25 records of 1 KiB, a table of 7 clusters)
/// and a place for a list: `lcn` is the first free cluster.
fn small_volume_with_free_clusters(spec: Spec) -> (Vec<u8>, u64) {
    let mut v = Volume::new(spec);
    v.file("a.bin", ROOT, 5_000);
    let lcn = v.alloc(1);
    (v.finish(), lcn)
}

// ---- the table's own attribute list ----

#[test]
fn the_tables_own_list_is_read_from_clusters_and_refused_beyond_the_cap() {
    // A stored list of a few entries that name nothing outside the first
    // record is fine.
    let (mut img, lcn) = small_volume_with_free_clusters(roomy());
    let mut entry = vec![0u8; 0x20];
    entry[0..4].copy_from_slice(&0x10u32.to_le_bytes());
    entry[4..6].copy_from_slice(&0x20u16.to_le_bytes());
    entry[7] = 0x1A;
    img[(lcn * CB) as usize..(lcn * CB) as usize + 0x20].copy_from_slice(&entry);
    replace_table_record(
        &mut img,
        1024,
        7,
        25 * 1024,
        vec![stored_list(0x20, lcn, 1)],
    );
    assert_eq!(refused(img.clone(), NtfsOptions::default()), None);

    // A list one byte past the cap, which the clusters it names cover and
    // which reads as a valid (empty) list: a reader without the cap takes it.
    let (mut img, lcn) = small_volume_with_free_clusters(roomy());
    let clusters = (MAX_LIST_BYTES + 1).div_ceil(CB);
    replace_table_record(
        &mut img,
        1024,
        7,
        25 * 1024,
        vec![stored_list(MAX_LIST_BYTES + 1, lcn, clusters)],
    );
    let why = refused(img, NtfsOptions::default()).expect("refused");
    assert!(why.contains("attribute list"), "{why}");

    // A list that claims what no volume holds is refused before anything is
    // allocated for it, and its runs cannot make it readable.
    let (mut img, lcn) = small_volume_with_free_clusters(roomy());
    replace_table_record(
        &mut img,
        1024,
        7,
        25 * 1024,
        vec![stored_list(1 << 62, lcn, 1)],
    );
    assert!(
        refused(img, NtfsOptions::default())
            .expect("refused")
            .contains("attribute list")
    );

    // A list whose clusters are not there is an unreadable table.
    let (mut img, _) = small_volume_with_free_clusters(roomy());
    replace_table_record(
        &mut img,
        1024,
        7,
        25 * 1024,
        vec![stored_list(0x40, 100_000, 1)],
    );
    assert!(refused(img, NtfsOptions::default()).is_some());
}

#[test]
fn the_table_may_name_so_many_extension_records_and_no_more() {
    let build = |names: u64| {
        let (mut img, lcn) = small_volume_with_free_clusters(roomy());
        let mut list = Vec::new();
        for k in 0..names {
            let mut e = vec![0u8; 0x20];
            e[0..4].copy_from_slice(&0x80u32.to_le_bytes());
            e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
            e[7] = 0x1A;
            // The pieces of one unnamed stream, each in its own record.
            e[8..16].copy_from_slice(&(7 + k).to_le_bytes());
            let r = 100 + k;
            e[16..24].copy_from_slice(&(r | (1u64 << 48)).to_le_bytes());
            list.extend(e);
        }
        let clusters = (list.len() as u64).div_ceil(CB);
        let at = (lcn * CB) as usize;
        img[at..at + list.len()].copy_from_slice(&list);
        // The list needs `clusters` clusters; the kit left one free, but
        // everything after it is free too.
        replace_table_record(
            &mut img,
            1024,
            7,
            25 * 1024,
            vec![stored_list(list.len() as u64, lcn, clusters)],
        );
        img
    };
    let limit = MAX_MFT_EXTENSIONS as u64;
    let over = refused(build(limit + 1), NtfsOptions::default()).expect("refused");
    assert!(over.contains("too many extension records"), "{over}");
    // At the limit the read goes on to the first extension record, which is
    // not in the table: refused for that reason instead.
    let at = refused(build(limit), NtfsOptions::default()).expect("refused");
    assert!(!at.contains("too many extension records"), "{at}");
}

#[test]
fn table_runs_that_overlap_or_leave_the_volume_are_refused() {
    let build = |runs: &[(Option<u64>, u64)]| {
        let (mut img, _) = small_volume_with_free_clusters(Spec::default());
        let covered: u64 = runs.iter().map(|r| r.1).sum();
        let rec = RecordBuilder::new(1024, 1)
            .number(0)
            .standard_info(0x6, 1_700_000_000)
            .file_name(5, 5, 3, "$MFT")
            .nonres(&NonRes {
                ty: 0x80,
                name: String::new(),
                flags: 0,
                lowest_vcn: 0,
                highest_vcn: covered - 1,
                unit: 0,
                allocated: covered * CB,
                size: covered * CB,
                initialized: covered * CB,
                compressed: None,
                runs: encode_runs(runs),
            });
        let at = (4 * CB) as usize;
        img[at..at + 1024].copy_from_slice(&rec.build());
        img
    };
    // The same seven clusters named three times: the records in them would be
    // read at three numbers each.
    let why = refused(
        build(&[(Some(4), 7), (Some(4), 7), (Some(4), 7)]),
        NtfsOptions::default(),
    )
    .expect("refused");
    assert!(why.contains("overlap"), "{why}");
    // Two runs that share only one cluster.
    let why = refused(
        build(&[(Some(4), 7), (Some(10), 7)]),
        NtfsOptions::default(),
    )
    .expect("refused");
    assert!(why.contains("overlap"), "{why}");
    // Runs that touch are a fragmented table, and fine.
    assert_eq!(
        refused(
            build(&[(Some(4), 7), (Some(11), 7)]),
            NtfsOptions::default()
        ),
        None
    );
    // A run past the end of the volume.
    let why = refused(
        build(&[(Some(4), 7), (Some(4000), 200)]),
        NtfsOptions::default(),
    )
    .expect("refused");
    assert!(why.contains("run list"), "{why}");
}

#[test]
fn the_tables_first_record_must_be_record_zero() {
    let (mut img, _) = small_volume_with_free_clusters(Spec::default());
    let at = (4 * CB) as usize;
    img[at + 0x2C..at + 0x30].copy_from_slice(&9u32.to_le_bytes());
    let why = refused(img, NtfsOptions::default()).expect("refused");
    assert!(why.contains("another record"), "{why}");
}

// ---- attribute lists of files ----

/// A file whose attribute list is stored in clusters, of the given size, over
/// the given region.
fn volume_with_stored_lists(sizes: &[u64], region_clusters: u64) -> Vec<u8> {
    let mut v = Volume::new(Spec {
        clusters: 20_000,
        ..Spec::default()
    });
    let lcn = v.alloc(region_clusters);
    for (i, size) in sizes.iter().enumerate() {
        let f = v.file(&format!("f{i}"), ROOT, 0);
        let rec = RecordBuilder::new(1024, 1)
            .standard_info(0x20, 1_700_000_000)
            .file_name(5, 5, 3, &format!("f{i}"))
            .raw(stored_list(*size, lcn, size.div_ceil(CB)))
            .build();
        v.raw.insert(f, rec);
    }
    v.finish()
}

#[test]
fn a_stored_list_over_the_size_cap_marks_its_file_and_is_not_read() {
    let size = MAX_LIST_BYTES + 1;
    let img = volume_with_stored_lists(&[size], size.div_ceil(CB));
    let (result, report) = read_ok(img);
    let m = flat(&result.snapshot);
    assert!(m["f0"].failed);
    assert_eq!(
        report.diagnostics.count(ProblemKind::AttributeListCorrupt),
        1
    );
    assert_eq!(report.lists_unchecked, 1);
    assert!(result.snapshot.root().unwrap().has_unreadable_below());
}

#[test]
fn a_list_at_the_size_cap_is_read() {
    let img = volume_with_stored_lists(&[MAX_LIST_BYTES], MAX_LIST_BYTES.div_ceil(CB));
    let (result, report) = read_ok(img);
    assert!(!flat(&result.snapshot)["f0"].failed);
    assert_eq!(report.diagnostics.total(), 0, "{:?}", report.diagnostics);
}

#[test]
fn stored_lists_are_read_only_up_to_the_total_budget() {
    // Five lists of 60 MiB over the same clusters: four fit the budget of
    // 256 MiB, the fifth does not.
    let each = 60u64 << 20;
    assert!(4 * each <= LIST_BUDGET_BYTES && 5 * each > LIST_BUDGET_BYTES);
    let img = volume_with_stored_lists(&[each; 5], each.div_ceil(CB));
    let (result, report) = read_ok(img);
    let m = flat(&result.snapshot);
    let damaged: Vec<&String> = m
        .iter()
        .filter(|(k, i)| k.starts_with('f') && i.failed)
        .map(|(k, _)| k)
        .collect();
    assert_eq!(damaged, vec![&"f4".to_string()], "{:?}", report.diagnostics);
    assert_eq!(report.diagnostics.count(ProblemKind::ReadBudgetExceeded), 1);
    // It is the budget that stopped it, not a read that failed.
    assert_eq!(
        report.diagnostics.count(ProblemKind::AttributeListCorrupt),
        0
    );
}

#[test]
fn a_list_larger_than_the_memory_that_is_left_is_not_allocated() {
    let size = 8u64 << 20;
    let img = volume_with_stored_lists(&[size], size.div_ceil(CB));
    // With a limit that the model alone nearly fills, the list is refused
    // before it is read.
    let (result, report) = scan_memory(
        img,
        NtfsOptions {
            threads: Some(1),
            max_memory_bytes: 6 << 20,
            ..NtfsOptions::default()
        },
    );
    let report = report.expect("the read itself fits the limit");
    assert!(flat(&result.snapshot)["f0"].failed);
    assert_eq!(report.diagnostics.count(ProblemKind::ReadBudgetExceeded), 1);
}

// ---- depth ----

#[test]
fn directories_nested_past_the_depth_limit_are_marked_and_the_rest_is_placed() {
    let n = MAX_DEPTH + 4;
    let mut v = Volume::new(Spec {
        record_bytes: 1024,
        cluster_bytes: 4096,
        clusters: 16_600,
        mft_area_clusters: 16_500,
        ..Spec::default()
    });
    let mut parent = ROOT;
    for i in 0..n {
        parent = v.dir(&format!("d{}", i % 10), parent);
    }
    v.file("leaf", parent, 5000);
    let (result, report) = read_ok(v.finish());
    assert_eq!(report.diagnostics.count(ProblemKind::TooDeep), 1);
    let root = result.snapshot.root().unwrap();
    assert!(root.has_unreadable_below());
    assert!(root.bytes_are_lower_bound());
    // The directory at the limit is in the tree and marked; the three below
    // it and the leaf are not reachable and are placed in the folder of
    // strays.
    assert_eq!(report.adopted.files, 4);
    assert_eq!(report.adopted.allocated, 2 * CB);
    assert_eq!(report.unreachable.allocated, 0);
}

// ---- names ----

fn dos_units(i: usize) -> Vec<u16> {
    vec![
        0x4E00 + (i % 20_000) as u16,
        0x4E00 + (i / 20_000) as u16,
        0x41,
    ]
}

/// One file whose attribute list names `k` extension records, each full of
/// DOS-namespace names. A filter that compares every name with every other
/// is quadratic in them; a file may not have that many names in any case.
fn file_with_many_names(k: usize) -> (Vec<u8>, usize) {
    let rb = 65_536usize;
    let mut v = Volume::new(Spec {
        record_bytes: rb as u32,
        cluster_bytes: 4096,
        clusters: 20_000,
        mft_area_clusters: 16 * (k as u64 + 30),
        ..Spec::default()
    });
    let base_no = 24u32;
    let mut list = Vec::new();
    for j in 0..k {
        let r = u64::from(base_no) + 1 + j as u64;
        let mut e = vec![0u8; 0x20];
        e[0..4].copy_from_slice(&0x30u32.to_le_bytes());
        e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
        e[7] = 0x1A;
        e[16..24].copy_from_slice(&(r | (1u64 << 48)).to_le_bytes());
        list.extend(e);
    }
    let base = RecordBuilder::new(rb, 1)
        .standard_info(0x20, 1_700_000_000)
        .file_name(5, 5, 1, "big")
        .resident(0x20, "", &list)
        .data_resident("", &[1u8; 10])
        .build();
    v.raw.insert(base_no, base);
    let mut names = 0usize;
    for j in 0..k {
        let mut ext = RecordBuilder::new(rb, 1).extension_of(u64::from(base_no), 1);
        let mut m = 0;
        while 0x138 + (m + 1) * 96 + 8 <= rb {
            ext = ext.file_name_units(5, 5, 2, &dos_units(names));
            names += 1;
            m += 1;
        }
        v.raw.insert(base_no + 1 + j as u32, ext.build());
    }
    (v.finish(), names)
}

#[test]
fn a_file_with_a_hostile_number_of_names_is_damaged_and_read_in_time() {
    // 54,320 names on one file: the quadratic filter took 2.6 s of release
    // time for this; the bounded one does not depend on it.
    let (img, names) = file_with_many_names(80);
    assert!(names > 50_000);
    let begun = Instant::now();
    let (result, report) = read_ok(img);
    let took = begun.elapsed();
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert!(report.diagnostics.count(ProblemKind::TooManyNames) > 0);
    let m = flat(&result.snapshot);
    // The file is in the tree and flagged; the names past the limit are
    // gone, and what is left of them is aliases of "big".
    assert!(m["big"].failed);
    assert_eq!(m.keys().filter(|k| k.as_str() == "big").count(), 1);
    assert!(result.snapshot.root().unwrap().has_unreadable_below());
    assert!(MAX_NAMES_PER_FILE < names);
}

#[test]
fn the_alias_filter_is_n_log_n_and_removes_exactly_the_aliases() {
    // (directory, is a DOS name)
    let mut items: Vec<(u64, bool)> = Vec::new();
    for d in 0..100_000u64 {
        items.push((d, false));
        items.push((d, true));
        // A DOS name alone in its directory stays.
        items.push((1_000_000 + d, true));
    }
    let begun = Instant::now();
    drop_dos_aliases(&mut items, |&(d, dos)| (d, dos));
    assert!(
        begun.elapsed() < Duration::from_secs(2),
        "{:?}",
        begun.elapsed()
    );
    assert_eq!(items.len(), 200_000);
    assert!(items.iter().filter(|i| i.1).all(|i| i.0 >= 1_000_000));
    // Nothing to do with fewer than two names.
    let mut one = vec![(1u64, true)];
    drop_dos_aliases(&mut one, |&(d, dos)| (d, dos));
    assert_eq!(one.len(), 1);
}

#[test]
fn many_hard_links_in_an_extension_record_are_kept() {
    // The kit's records hold about 650 short names each; a file with that
    // many links, most of them in an extension record, is not damaged.
    let mut v = Volume::new(Spec {
        record_bytes: 65_536,
        cluster_bytes: 4096,
        clusters: 4_000,
        mft_area_clusters: 1_000,
        ..Spec::default()
    });
    let f = v.file("f", ROOT, 10);
    for i in 0..600 {
        v.link(f, &format!("l{i}"), ROOT);
    }
    v.with_extension(f, false);
    let (result, report) = read_ok(v.finish());
    assert_eq!(report.diagnostics.count(ProblemKind::TooManyNames), 0);
    let m = flat(&result.snapshot);
    assert!(m.contains_key("l0") && m.contains_key("l599") && m.contains_key("f"));
    assert!(!m["f"].failed);
}
