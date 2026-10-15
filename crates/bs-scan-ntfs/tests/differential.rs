#![cfg(target_os = "linux")]
//! The reader against the walker, on NTFS images made by ntfs-3g, and
//! against `ntfsinfo`, which reads the same images with an independent
//! implementation.
//!
//! The walker scans the image through a read-only ntfs-3g mount (remounted
//! so that nothing cached by the writing mount is used). The reader scans the
//! unmounted image, once counting only what a directory listing reports
//! ([`Accounting::Listing`]), which must equal the walker's tree path for
//! path, and once counting everything ([`Accounting::Full`]), which must
//! differ from the first by exactly what the report says it adds.
//!
//! Allowed differences are in [`EXCEPTIONS`]. Each has a reason, and a check
//! below that the reason is true of every difference it covers. A difference
//! that is not covered is a finding, not something to add to the table.
//!
//! Needs `mkntfs`, `ntfs-3g` and `ntfsinfo` (package ntfs-3g) and `/dev/fuse`.
mod common;

use bs_scan::{ScanOptions, ScanResult, Walker, start};
use bs_scan_ntfs::{Accounting, NtfsOptions, NtfsReport, NtfsScanner};
use bs_tree::{NodeFlags, NodeKind};
use common::compare::*;
use common::linux::*;
use common::testkit::flatten;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const R_SYSTEM: &str = "The system files ($MFT, $LogFile, $Bitmap, $Extend ...) are hidden by the \
file system driver, so no walker can list them; they exist only for a reader of the \
table.";
const R_INTERIX: &str = "ntfs-3g stores a symbolic link as an ordinary file holding an Interix \
link record, which Windows treats as a plain file; the walker through ntfs-3g reports a \
link, the reader a file.";
const R_RESIDENT: &str = "Data kept inside the file record occupies no cluster. A Windows listing \
reports its length rounded up to eight bytes as the allocation, which the listing accounting \
follows; ntfs-3g reports whole 512-byte blocks in st_blocks, and the walker reads that.";

const R_DIRECTORY: &str = "A directory's own bytes are its index buffers. The walker through ntfs-3g \
reports them (st_blocks of a directory); a Windows directory listing reports no size for a \
directory, which is what the listing accounting follows. Full accounting counts them.";

/// Differences between the reader's tree and the walker's that are not bugs.
const EXCEPTIONS: &[Exception] = &[
    Exception {
        field: "presence",
        reason: R_SYSTEM,
        covers: |path, _| is_system_path(path),
    },
    Exception {
        field: "kind",
        reason: R_INTERIX,
        covers: |path, _| SYMLINKS.contains(&path),
    },
    Exception {
        field: "allocated",
        reason: R_DIRECTORY,
        covers: |_, info| info.kind == NodeKind::Dir,
    },
    Exception {
        field: "allocated",
        reason: R_RESIDENT,
        covers: |_, info| info.kind == NodeKind::File && info.logical < 1024,
    },
];

/// The fixture paths that are symbolic links on the Linux side.
const SYMLINKS: &[&str] = &["sym", "docs/sym2"];

const SYSTEM_NAMES: &[&str] = &[
    "$MFT", "$MFTMirr", "$LogFile", "$Volume", "$AttrDef", "$Bitmap", "$Boot", "$BadClus",
    "$Secure", "$UpCase", "$Extend",
];

fn is_system_path(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or("");
    SYSTEM_NAMES.contains(&first)
}

struct Scenario {
    fixture: Fixture,
    walker: ScanResult,
    listing: ScanResult,
    listing_report: NtfsReport,
    full: ScanResult,
    full_report: NtfsReport,
    mount_path: PathBuf,
}

fn read_image(
    img: &Path,
    root: &Path,
    accounting: Accounting,
    threads: Option<usize>,
) -> (ScanResult, NtfsReport) {
    let scanner = Arc::new(NtfsScanner::new(
        img,
        NtfsOptions {
            accounting,
            threads,
            path_separator: b'/',
            ..NtfsOptions::default()
        },
    ));
    let result = start(scanner.clone(), root.to_path_buf(), ScanOptions::default()).wait();
    assert_sound(&result);
    (result, scanner.last_report().expect("a report"))
}

fn scenario(name: &str, spec: &ImageSpec) -> Scenario {
    let mut fixture = build(name, spec);
    fixture.unmount();
    fixture.remount_read_only();
    let mount_path = fixture.mount_point().to_path_buf();
    let walker = Walker::scan_blocking(&mount_path, ScanOptions::default());
    assert_sound(&walker);
    fixture.unmount();
    let (listing, listing_report) =
        read_image(&fixture.img, &mount_path, Accounting::Listing, Some(2));
    let (full, full_report) = read_image(&fixture.img, &mount_path, Accounting::Full, Some(2));
    Scenario {
        fixture,
        walker,
        listing,
        listing_report,
        full,
        full_report,
        mount_path,
    }
}

fn numbers(s: &str) -> Vec<u64> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter_map(|p| p.parse().ok())
        .collect()
}

impl Scenario {
    /// Reader (listing) against walker, with the exceptions applied and
    /// their claims checked against `ntfsinfo`.
    fn listing_equals_walker(&self) {
        let diffs = compare_trees(&self.listing.snapshot, &self.walker.snapshot);
        let (allowed, rest) = apply_exceptions(
            diffs,
            &[&self.listing.snapshot, &self.walker.snapshot],
            EXCEPTIONS,
        );
        assert_no_diffs("reader (listing) vs walker", &rest);
        for (d, reason) in &allowed {
            if *reason == R_SYSTEM {
                assert_eq!(d.field, "presence");
                assert_eq!(d.reader, "present", "{d:?}: system files are the reader's");
            } else if *reason == R_INTERIX {
                // The record the walker calls a link is a file of 8 + 2n
                // bytes: the Interix header and the target in UTF-16.
                let streams = oracle(&self.fixture.img, &format!("/{}", d.path));
                let (size, _) = listing_sizes(&streams).expect("a data stream");
                assert!(size >= 10 && (size - 8) % 2 == 0, "{d:?}: {size}");
                assert_eq!((d.reader.as_str(), d.other.as_str()), ("File", "Symlink"));
            } else if *reason == R_DIRECTORY {
                let want = index_allocation(&self.fixture.img, &format!("/{}", d.path));
                assert_eq!(numbers(&d.reader), vec![0], "{d:?}");
                assert_eq!(numbers(&d.other), vec![want], "{d:?}: the index buffers");
            } else if *reason == R_RESIDENT {
                let streams = oracle(&self.fixture.img, &format!("/{}", d.path));
                let unnamed = streams.iter().find(|s| s.name.is_empty()).expect("data");
                assert!(unnamed.resident, "{d:?}: not resident");
                let rounded = (unnamed.size + 7) & !7;
                assert_eq!(numbers(&d.reader), vec![rounded], "{d:?}");
                assert_eq!(
                    numbers(&d.other),
                    vec![rounded.div_ceil(512) * 512],
                    "{d:?}"
                );
            } else {
                panic!("an exception without a check: {reason}");
            }
        }
    }

    /// Every plain path against `ntfsinfo`.
    fn listing_equals_the_oracle(&self, every: usize) {
        let infos = flatten(&self.listing.snapshot);
        let mut checked = 0usize;
        for (n, (path, info)) in infos.iter().enumerate() {
            if n % every != 0 {
                continue;
            }
            let name = String::from_utf8_lossy(path).into_owned();
            if is_system_path(&name) || info.kind != NodeKind::File {
                continue;
            }
            if info.flags.contains(NodeFlags::HARD_LINK) {
                continue;
            }
            let streams = oracle(&self.fixture.img, &format!("/{name}"));
            let Some((size, allocated)) = listing_sizes(&streams) else {
                continue;
            };
            assert_eq!(
                (info.logical, info.allocated),
                (size, allocated),
                "{name}: reader vs ntfsinfo"
            );
            checked += 1;
        }
        assert!(checked > 0, "the oracle looked at nothing");
    }

    /// The named data streams of every file in the tree, system files
    /// included, once per file, from `ntfsinfo`: how many, their sizes, and
    /// the clusters they hold.
    fn expected_streams(&self) -> (u64, u64, u64) {
        let mut seen = std::collections::BTreeSet::new();
        let (mut count, mut logical, mut allocated) = (0u64, 0u64, 0u64);
        for (path, info) in flatten(&self.full.snapshot) {
            if info.kind == NodeKind::Dir && !is_system_path(&String::from_utf8_lossy(&path)) {
                // Directories can have streams too; ask about them as well.
            }
            let name = String::from_utf8_lossy(&path).into_owned();
            let (inode, streams) = oracle_with_inode(&self.fixture.img, &format!("/{name}"));
            if !seen.insert(inode) {
                continue;
            }
            for st in streams.iter().filter(|s| !s.name.is_empty()) {
                count += 1;
                logical += st.size;
                // The list of bad clusters is a stream as long as the volume
                // whose run list is a single hole: its header says it is
                // allocated, and no cluster is.
                let bad_list = name == "$BadClus" && st.name == "$Bad";
                allocated += if bad_list { 0 } else { occupied(st) };
            }
        }
        // The root directory.
        let (inode, streams) = oracle_with_inode(&self.fixture.img, "/");
        if seen.insert(inode) {
            for st in streams.iter().filter(|s| !s.name.is_empty()) {
                count += 1;
                logical += st.size;
                allocated += occupied(st);
            }
        }
        (count, logical, allocated)
    }

    /// The bytes that a listing reports for data kept inside file records
    /// (their length rounded up to eight), once per file, from `ntfsinfo`.
    /// Full accounting counts none of them: they are part of the table.
    fn resident_listing_bytes(&self) -> u64 {
        let mut seen = std::collections::BTreeSet::new();
        let mut total = 0u64;
        for (path, info) in flatten(&self.full.snapshot) {
            if info.kind != NodeKind::File {
                continue;
            }
            let name = String::from_utf8_lossy(&path).into_owned();
            let (inode, streams) = oracle_with_inode(&self.fixture.img, &format!("/{name}"));
            if !seen.insert(inode) {
                continue;
            }
            if let Some((_, allocated)) = listing_sizes(&streams)
                && streams.iter().any(|s| s.name.is_empty() && s.resident)
            {
                total += allocated;
            }
        }
        total
    }

    /// Full accounting differs from listing accounting by exactly what the
    /// report says it adds, less the resident data it does not count.
    fn full_adds_exactly_the_extras(&self) {
        let a = flatten(&self.listing.snapshot);
        let b = flatten(&self.full.snapshot);
        assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>());
        for (path, x) in &a {
            let y = &b[path];
            assert!(y.logical >= x.logical, "{path:?}");
            assert_eq!((x.kind, x.mtime, x.category), (y.kind, y.mtime, y.category));
        }
        let r = self.full_report.extras;
        let root = |s: &ScanResult| {
            let n = s.snapshot.root().expect("root");
            (n.logical(), n.allocated())
        };
        let (l0, a0) = root(&self.listing);
        let (l1, a1) = root(&self.full);
        assert_eq!(l1 - l0, r.stream_logical);
        let resident = self.resident_listing_bytes();
        assert_eq!(
            i128::from(a1) - i128::from(a0),
            i128::from(r.stream_allocated + r.attribute_allocated) - i128::from(resident),
            "allocated: full {a1}, listing {a0}, resident {resident}"
        );
        // The two reports describe one volume.
        assert_eq!(self.listing_report.extras, r);
        assert_eq!(
            self.listing_report.unreachable,
            self.full_report.unreachable
        );
        assert_eq!(
            self.listing_report.files_in_use,
            self.full_report.files_in_use
        );
    }
}

fn mixed() -> Vec<Item> {
    vec![
        dir("docs"),
        dir("docs/empty"),
        file("docs/readme.txt", 300),
        file("docs/photo.jpg", 70_000),
        file("docs/zero", 0),
        file("docs/exactly_cluster", 4096),
        file("docs/one_over", 4097),
        dir("links"),
        file("links/original", 50_000),
        link("links/second", "links/original"),
        link("docs/third", "links/original"),
        sparse("sparse.bin", 5 << 20, &[(0, 4096), (409_600, 12_288)]),
        compressed("comp/comp.txt", 300_000),
        stream("docs/readme.txt", "note", 19),
        stream("links/original", "big", 3000),
        symlink_to("sym", "docs"),
        dir("a/b/c/d/e"),
        file("a/b/c/d/e/leaf.txt", 5000),
    ]
}

#[test]
fn a_mixed_tree_matches_the_walker_and_the_oracle() {
    let s = scenario("mixed", &ImageSpec::new(mixed()));
    s.listing_equals_walker();
    s.listing_equals_the_oracle(1);
    s.full_adds_exactly_the_extras();
    // The special files were really there.
    let m = flatten(&s.listing.snapshot);
    assert!(
        m[b"sparse.bin".as_slice()]
            .flags
            .contains(NodeFlags::SPARSE_OR_COMPRESSED)
    );
    assert!(
        m[b"comp/comp.txt".as_slice()]
            .flags
            .contains(NodeFlags::SPARSE_OR_COMPRESSED)
    );
    assert!(
        m[b"comp/comp.txt".as_slice()].allocated < 300_000,
        "compressed on disk"
    );
    assert!(
        m[b"links/second".as_slice()]
            .flags
            .contains(NodeFlags::HARD_LINK)
    );
    let (count, logical, allocated) = s.expected_streams();
    assert!(count >= 2);
    assert_eq!(s.full_report.extras.streams, count);
    assert_eq!(s.full_report.extras.stream_logical, logical);
    assert_eq!(s.full_report.extras.stream_allocated, allocated);
    // The two named streams of the fixture are among them.
    assert!(logical >= 19 + 3000);
    assert_eq!(s.full_report.lost_records, 0);
    assert_eq!(
        s.full_report.diagnostics.total(),
        0,
        "{:?}",
        s.full_report.diagnostics
    );
    let _ = &s.mount_path;
}

/// The checks every scenario shares.
fn check_all(s: &Scenario, oracle_every: usize) {
    s.listing_equals_walker();
    s.listing_equals_the_oracle(oracle_every);
    assert_eq!(s.full_report.lost_records, 0);
    assert_eq!(
        s.full_report.diagnostics.total(),
        0,
        "{:?}",
        s.full_report.diagnostics
    );
    assert_eq!(s.listing_report.diagnostics.total(), 0);
}

#[test]
fn every_cluster_and_sector_size_matches() {
    for (cluster, sector) in [
        (512, 512),
        (1024, 512),
        (2048, 512),
        (8192, 512),
        (4096, 4096),
        (65_536, 512),
    ] {
        let items = vec![
            dir("d"),
            file("d/a", 700),
            file("d/b", 5_000),
            file("d/c", 100_000),
            link("d/c2", "d/c"),
            sparse("s", 1 << 20, &[(0, 1000), (700_000, 20_000)]),
            compressed("z/z.txt", 200_000),
            file("empty", 0),
        ];
        let spec = ImageSpec::new(items)
            .cluster(cluster)
            .sector(sector)
            .size_mb(48);
        let s = scenario(&format!("geo-{cluster}-{sector}"), &spec);
        assert_eq!(s.full_report.cluster_bytes, cluster, "{cluster}/{sector}");
        check_all(&s, 1);
        s.full_adds_exactly_the_extras();
    }
}

#[test]
fn large_directories_use_index_buffers_that_are_counted() {
    let mut items = vec![dir("big"), dir("big/sub")];
    for i in 0..1500 {
        items.push(file(
            &format!("big/file-with-a-longish-name-{i:05}.dat"),
            (i % 7) as u64 * 300,
        ));
    }
    let s = scenario("bigdir", &ImageSpec::new(items));
    check_all(&s, 37);
    s.full_adds_exactly_the_extras();
    let m = flatten(&s.full.snapshot);
    let big = &m[b"big".as_slice()];
    let want = attribute_allocation(&s.fixture.img, "/big");
    assert!(
        want >= 4096 * 8,
        "the directory should have grown buffers: {want}"
    );
    assert_eq!(
        big.allocated, want,
        "a directory's own bytes are its index buffers"
    );
    assert_eq!(big.logical, 0);
    assert!(s.full_report.extras.directory_allocated >= want);
}

#[test]
fn attribute_lists_and_extension_records() {
    let mut items = vec![
        dir("d"),
        file("many-streams", 20_000),
        file("many-names", 9_000),
    ];
    for i in 0..60 {
        items.push(stream("many-streams", &format!("stream{i:02}"), 150 + i));
    }
    for i in 0..45 {
        items.push(link(
            &format!("d/a-fairly-long-hard-link-name-number-{i:02}"),
            "many-names",
        ));
    }
    let s = scenario("lists", &ImageSpec::new(items));
    check_all(&s, 1);
    s.full_adds_exactly_the_extras();
    // The real thing was exercised: ntfs-3g spilled both files into
    // extension records behind attribute lists.
    let (_, list) = name_and_list_counts(&s.fixture.img, "/many-streams");
    assert!(list, "many-streams should have an attribute list");
    let (names, list) = name_and_list_counts(&s.fixture.img, "/many-names");
    assert!(list && names >= 46, "{names} names, list {list}");
    assert!(
        s.full_report.attribute_lists >= 2,
        "{:?}",
        s.full_report.attribute_lists
    );
    assert!(s.full_report.extension_records >= 2);
    let m = flatten(&s.full.snapshot);
    assert_eq!(
        m[b"many-streams".as_slice()].logical,
        20_000 + (0..60).map(|i| 150 + i).sum::<u64>()
    );
    let group: Vec<_> = m
        .iter()
        .filter(|(p, _)| p.starts_with(b"d/a-fairly") || p.as_slice() == b"many-names")
        .collect();
    assert_eq!(group.len(), 46);
    assert!(
        group
            .iter()
            .all(|(_, i)| i.flags.contains(NodeFlags::HARD_LINK))
    );
    assert_eq!(group.iter().map(|(_, i)| i.logical).sum::<u64>(), 9_000);
}

#[test]
fn names_of_every_kind() {
    let long = "l".repeat(255);
    let unicode = "caf\u{e9}-\u{65e5}\u{672c}\u{8a9e}-\u{1F600}-\u{10348}";
    let mut items = vec![
        dir("u"),
        file(&format!("u/{long}"), 10),
        file(&format!("u/{unicode}"), 20),
        file("u/with space.TXT", 30),
        file("u/trailing.dot.", 40),
        file("u/new\nline", 50),
        file("u/UPPER.JPG", 5_000),
        file("u/upper.jpg", 6_000),
        file("u/.hidden", 70),
    ];
    let mut deep = String::new();
    for i in 0..60 {
        deep.push_str(&format!("level{i}/"));
        items.push(dir(deep.trim_end_matches('/')));
    }
    items.push(file(&format!("{deep}bottom.txt"), 1234));
    let s = scenario("names", &ImageSpec::new(items));
    check_all(&s, 1);
    let m = flatten(&s.listing.snapshot);
    assert!(m.contains_key(format!("u/{long}").as_bytes()));
    assert!(m.contains_key(format!("u/{unicode}").as_bytes()));
    assert!(m.contains_key(b"u/new\nline".as_slice()));
    assert!(m.contains_key(format!("{deep}bottom.txt").as_bytes()));
    // Two names that differ only in case are two files.
    assert!(m.contains_key(b"u/UPPER.JPG".as_slice()) && m.contains_key(b"u/upper.jpg".as_slice()));
}

#[test]
fn many_files_across_several_chunks_with_any_number_of_threads() {
    let mut items = Vec::new();
    for d in 0..12 {
        items.push(dir(&format!("d{d}")));
    }
    for i in 0..9500 {
        items.push(file(
            &format!("d{}/f{i}.bin", i % 12),
            (i % 11) as u64 * 200,
        ));
    }
    let s = scenario("chunks", &ImageSpec::new(items).size_mb(80));
    assert!(s.full_report.records > 9500, "{}", s.full_report.records);
    // More than one 4 MiB chunk of the table.
    assert!(s.full_report.mft_bytes_read > (4 << 20) * 2);
    check_all(&s, 997);
    let reference = flatten(&s.listing.snapshot);
    for threads in [Some(1), Some(3), Some(8)] {
        let (again, report) =
            read_image(&s.fixture.img, &s.mount_path, Accounting::Listing, threads);
        assert_eq!(flatten(&again.snapshot), reference, "{threads:?}");
        assert_eq!(report.files_in_use, s.listing_report.files_in_use);
    }
}
