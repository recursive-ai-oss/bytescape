#![cfg(windows)]
//! The reader against the walker on real NTFS volumes, on Windows: a virtual
//! disk filled with every kind of entry, and the machine's own system drive.
//!
//! The walker lists the mounted volume through the file system; the reader
//! reads the volume's table through `\\.\X:`. They must agree path by path
//! except where `EXCEPTIONS` says why not. The virtual-disk test needs
//! administrator rights and `BYTESCAPE_DISPOSABLE_HOST=1`, because it attaches
//! disks; the system-drive test also needs `BYTESCAPE_NTFS_LIVE=1`, because it
//! takes minutes. Without the variables they say `NOT RUN`.
//!
//! The system-drive comparison fails on any difference that no evidence
//! explains: every difference is sorted into a cause (see `common::live`),
//! and each cause rests on facts about that very path, such as the system's
//! own answer when the entry is opened. `BYTESCAPE_NTFS_LIVE=report` prints
//! the sorted differences and does not fail on the unexplained ones (for
//! finding out what a new class of difference is); any other value is the
//! strict default. Every phase of both tests runs under a
//! time bound that names the phase; the walk of the system drive, which is slow
//! when cold, is bounded by lack of progress instead, and prints its count.
mod common;

use bs_scan::{ScanOptions, ScanResult, Walker, start};
use bs_scan_ntfs::{Accounting, NtfsOptions, NtfsReport, NtfsScanner};
use bs_tree::{NodeFlags, NodeKind};
use common::compare::*;
use common::live::{self, SYSTEM_NAMES, first_part};
use common::testkit::{Info, flatten};
use common::truth::SystemOracle;
use common::watchdog::*;
use common::windows::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const R_SYSTEM: &str = "The system files ($MFT, $LogFile, $Bitmap, $Extend ...) are hidden by the \
file system, so no walker can list them; they exist only for a reader of the table.";
const R_LINK_TIME: &str = "A hard-linked file has one modification time, kept with the file; each \
directory entry keeps its own copy, updated only through that name, and a listing reports \
the entry's copy. The reader reports the file's.";
const R_SPARSE: &str = "A directory entry's copy of a sparse file's allocated size is not kept \
current by the file system (a listing reports 0 for a file with data in it); the reader \
reports the clusters the file's run list backs.";

/// Differences between the reader's tree and the walker's that are not bugs.
const EXCEPTIONS: &[Exception] = &[
    Exception {
        field: "presence",
        reason: R_SYSTEM,
        covers: |path, _| SYSTEM_NAMES.contains(&first_part(path)),
    },
    Exception {
        field: "mtime",
        reason: R_LINK_TIME,
        covers: |_, info| info.flags.contains(NodeFlags::HARD_LINK),
    },
    Exception {
        field: "allocated",
        reason: R_SPARSE,
        covers: |_, info| {
            info.kind == NodeKind::File && info.flags.contains(NodeFlags::SPARSE_OR_COMPRESSED)
        },
    },
    SYSTEM_FOLDER_TIME,
];

/// What the system itself says about files, read by opening them (which is
/// how an ordinary program asks): the last write time and the clusters held.
/// `paths` are relative, `/`-separated and plain ASCII.
fn system_truth(root: &Path, paths: &[String]) -> std::collections::BTreeMap<String, (i64, u64)> {
    let list = std::env::temp_dir().join(format!("bsntfs-truth-{}.txt", std::process::id()));
    let prefix = root.display().to_string();
    let lines: Vec<String> = paths
        .iter()
        .map(|p| format!("{prefix}{}", p.replace('/', "\\")))
        .collect();
    std::fs::write(&list, lines.join("\n")).expect("write the list");
    let script = format!(
        "Add-Type -Namespace B -Name N -MemberDefinition '[DllImport(\"kernel32.dll\", CharSet=CharSet.Unicode)] public static extern uint GetCompressedFileSizeW(string name, out uint high);'; \
         foreach ($p in Get-Content -LiteralPath '{}') {{ $h=[uint32]0; $lo=[B.N]::GetCompressedFileSizeW($p, [ref]$h); \
         $size=([uint64]$h * 4294967296) + $lo; $t=[DateTimeOffset](Get-Item -LiteralPath $p).LastWriteTimeUtc; \
         '{{0}}|{{1}}|{{2}}' -f $p, $t.ToUnixTimeSeconds(), $size }}",
        list.display()
    );
    let text =
        run(std::process::Command::new("powershell").args(["-NoProfile", "-Command", &script]));
    std::fs::remove_file(&list).ok();
    text.lines()
        .filter_map(|l| {
            let mut it = l.trim().split('|');
            let p = it.next()?.strip_prefix(&prefix)?.replace('\\', "/");
            Some((p, (it.next()?.parse().ok()?, it.next()?.parse().ok()?)))
        })
        .collect()
}

/// Checks that each allowed difference is what its reason says.
fn verify_exceptions(root: &Path, allowed: &[(Diff, &'static str)]) {
    let paths: Vec<String> = allowed
        .iter()
        .filter(|(_, r)| *r == R_LINK_TIME || *r == R_SPARSE)
        .map(|(d, _)| d.path.clone())
        .collect();
    assert!(paths.iter().all(|p| p.is_ascii()), "{paths:?}");
    let truth = system_truth(root, &paths);
    for (d, reason) in allowed {
        let number = |s: &str| s.parse::<i64>().expect("a number");
        if *reason == R_LINK_TIME {
            let (mtime, _) = truth[&d.path];
            assert_eq!(number(&d.reader), mtime, "{d:?}: the file's own time");
        } else if *reason == R_SPARSE {
            let (_, held) = truth[&d.path];
            assert_eq!(
                number(&d.reader) as u64,
                held,
                "{d:?}: the clusters the file holds"
            );
        } else if *reason == R_SYSTEM_FOLDER_TIME {
            // Ask the system what it says now, by opening the folder the way
            // an ordinary program does. If it will not say, the direction of
            // the difference is all there is to check, and that is printed.
            let restat = std::fs::metadata(root.join(&d.path))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|t| t.as_secs() as i64);
            if restat.is_none() {
                println!("the system would not give the time of {:?} again", d.path);
            }
            if let Err(why) = check_system_folder_time(d, restat) {
                panic!("{why}");
            }
        } else {
            assert_eq!(*reason, R_SYSTEM, "an exception without a check");
            assert_eq!(d.reader, "present");
        }
    }
}

fn read_volume(
    raw: &Path,
    root: &Path,
    accounting: Accounting,
    threads: Option<usize>,
) -> (ScanResult, NtfsReport) {
    let scanner = Arc::new(NtfsScanner::new(
        raw,
        NtfsOptions {
            accounting,
            threads,
            ..NtfsOptions::default()
        },
    ));
    let result = start(scanner.clone(), root.to_path_buf(), ScanOptions::default()).wait();
    assert_sound(&result);
    (result, scanner.last_report().expect("a report"))
}

fn canonical(drive: &Path) -> PathBuf {
    std::fs::canonicalize(drive).expect("canonical drive root")
}

fn items() -> Vec<Item> {
    let long = "n".repeat(255);
    let mut items = vec![
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
        Item::Sparse(
            "sparse.bin".into(),
            5 << 20,
            vec![(0, 4096), (409_600, 12_288)],
        ),
        Item::Compressed("comp/comp.txt".into(), 300_000),
        stream("docs/readme.txt", "note", 19),
        stream("links/original", "big", 3000),
        Item::Junction("junction".into(), "docs".into()),
        Item::Symlink("filelink".into(), "docs/readme.txt".into(), false),
        Item::Symlink("dirlink".into(), "docs".into(), true),
        Item::Offline("docs/offline.docx".into(), 8000),
        dir("u"),
        file(&format!("u/{long}"), 10),
        file("u/caf\u{e9} \u{65e5}\u{672c} \u{1F600}", 20),
    ];
    // A name with an unpaired surrogate is legal on NTFS and in no UTF-8.
    let mut lone = wide("lone-");
    lone.push(0xD800);
    lone.push(u16::from(b'x'));
    items.push(Item::RawName("u".into(), lone, 30));
    let mut deep = String::new();
    for i in 0..40 {
        deep.push_str(&format!("level{i}/"));
        items.push(dir(deep.trim_end_matches('/')));
    }
    items.push(file(&format!("{deep}bottom.txt"), 1234));
    items.push(dir("big"));
    for i in 0..1500 {
        items.push(file(
            &format!("big/file-with-a-longish-name-{i:05}.dat"),
            (i % 7) as u64 * 300,
        ));
    }
    items
}

/// The tests that attach disks or flush volume caches take turns: the
/// storage services they use can wait on each other.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_virtual_disk_matches_the_walker() {
    if !allowed("a_virtual_disk_matches_the_walker") {
        return;
    }
    let _turn = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let vhd = phase("create the virtual disk", PREPARE_LIMIT, || {
        Vhd::create("match", 256, None)
    });
    let root = vhd.root();
    let filled = root.clone();
    phase("fill the disk", PREPARE_LIMIT, move || {
        populate(&filled, &items());
    });
    let letter = vhd.letter;
    phase("flush the volume", FLUSH_LIMIT, move || {
        flush_volume(letter)
    });
    let canon = canonical(&root);
    let walked = canon.clone();
    let walker = phase("walk the volume", WALK_LIMIT, move || {
        Walker::scan_blocking(&walked, ScanOptions::default())
    });
    assert_sound(&walker);
    let (raw, canon_for_read) = (vhd.raw(), canon.clone());
    let (listing, listing_report) = phase("read the table (listing)", READ_LIMIT, move || {
        read_volume(&raw, &canon_for_read, Accounting::Listing, Some(2))
    });
    let (raw, canon_for_read) = (vhd.raw(), canon.clone());
    let (full, full_report) = phase("read the table (full)", READ_LIMIT, move || {
        read_volume(&raw, &canon_for_read, Accounting::Full, Some(2))
    });
    println!("{full_report:#?}");

    let (allowed_diffs, rest) = phase("compare the trees", COMPARE_LIMIT, {
        let (l, w) = (listing.snapshot.clone(), walker.snapshot.clone());
        move || apply_exceptions(compare_trees(&l, &w), &[&l, &w], EXCEPTIONS)
    });
    for (d, reason) in &allowed_diffs {
        println!(
            "allowed {:?} {}: reader {} / walker {}  ({reason})",
            d.path, d.field, d.reader, d.other
        );
    }
    assert_no_diffs("reader (listing) vs walker", &rest);
    verify_exceptions(&root, &allowed_diffs);

    // What the fixture says, independent of both.
    let m = flatten(&full.snapshot);
    let l = flatten(&listing.snapshot);
    let at = |m: &std::collections::BTreeMap<Vec<u8>, Info>, p: &str| m.get(p.as_bytes()).cloned();
    if let Some(i) = at(&l, "docs/exactly_cluster") {
        assert_eq!((i.logical, i.allocated), (4096, 4096));
    }
    if let Some(i) = at(&l, "docs/one_over") {
        assert_eq!((i.logical, i.allocated), (4097, 8192));
    }
    if let Some(i) = at(&l, "docs/zero") {
        assert_eq!((i.logical, i.allocated), (0, 0));
    }
    let sparse = at(&l, "sparse.bin").expect("sparse.bin");
    assert_eq!(sparse.logical, 5 << 20);
    // Two islands of data, each inside one 64 KiB unit of a sparse file.
    assert_eq!(sparse.allocated, 2 * 65_536);
    assert!(sparse.flags.contains(NodeFlags::SPARSE_OR_COMPRESSED));
    let comp = at(&l, "comp/comp.txt").expect("comp.txt");
    assert_eq!(comp.logical, 300_000);
    assert!(
        comp.allocated < 150_000,
        "compressed on disk: {}",
        comp.allocated
    );
    assert!(comp.flags.contains(NodeFlags::SPARSE_OR_COMPRESSED));
    let junction = at(&l, "junction").expect("junction");
    assert_eq!(junction.kind, NodeKind::Other);
    assert!(junction.flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(
        !l.contains_key(b"junction/readme.txt".as_slice()),
        "a junction is never entered"
    );
    let offline = at(&l, "docs/offline.docx").expect("offline");
    assert!(offline.flags.contains(NodeFlags::CLOUD_PLACEHOLDER));
    assert_eq!(offline.logical, 8000);
    for n in ["links/original", "links/second", "docs/third"] {
        assert!(l[n.as_bytes()].flags.contains(NodeFlags::HARD_LINK), "{n}");
    }
    // Alternate streams are in the full count and not in the listing.
    assert_eq!(m[b"docs/readme.txt".as_slice()].logical, 300 + 19);
    assert_eq!(l[b"docs/readme.txt".as_slice()].logical, 300);
    assert!(full_report.extras.streams >= 2);
    assert!(full_report.extras.stream_logical >= 3019);
    // The two scans of the same volume agree about everything but extras.
    assert_eq!(listing_report.files_in_use, full_report.files_in_use);
    assert_eq!(full_report.lost_records, 0);
    assert_eq!(
        full_report.diagnostics.total(),
        0,
        "{:?}",
        full_report.diagnostics
    );
    // Any number of threads gives the same tree.
    let reference = flatten(&listing.snapshot);
    for threads in [Some(1), Some(8)] {
        let (raw, canon) = (vhd.raw(), canon.clone());
        let (again, _) = phase("read the table again", READ_LIMIT, move || {
            read_volume(&raw, &canon, Accounting::Listing, threads)
        });
        assert_eq!(flatten(&again.snapshot), reference, "{threads:?}");
    }
}

/// How far before the flush a change still counts as inside the scan's
/// window: file times are kept to the second and set by the file system a
/// moment after the change.
const LIVE_SLACK_SECS: i64 = 5;

/// How long the system may be asked about the differences: one open per
/// path, a few hundred thousand at most.
const ASK_LIMIT: std::time::Duration = std::time::Duration::from_secs(1800);

/// The machine's own drive is a volume that is changing while it is read.
/// Every difference is sorted into a cause that evidence backs for that path
/// (see `common::live`), the sorted differences are printed, and one that no
/// cause explains fails the test.
#[test]
fn the_system_drive_matches_the_walker_up_to_live_changes() {
    if !allowed("the_system_drive_matches_the_walker_up_to_live_changes") {
        return;
    }
    let live = std::env::var("BYTESCAPE_NTFS_LIVE").ok();
    if live.is_none() {
        println!(
            "NOT RUN: the system drive comparison needs BYTESCAPE_NTFS_LIVE=1 (it takes minutes)"
        );
        return;
    }
    let strict = live.as_deref() != Some("report");
    let _turn = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let letter = drive.chars().next().expect("a drive letter");
    let root = PathBuf::from(format!("{drive}\\"));
    let canon = canonical(&root);
    // Changes made from here on may be missing from the reader's view of the
    // disk, or differ from the walker's.
    let window_from = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
        - LIVE_SLACK_SECS;
    phase("flush the system drive", FLUSH_LIMIT, move || {
        flush_volume(letter)
    });
    // The walk is watched for lack of progress rather than for total time:
    // the first pass over a large cold drive is slow, and a progress line
    // every minute shows how slow, and whether it is moving at all.
    let handle = start(Arc::new(Walker), canon.clone(), ScanOptions::default());
    watch("walk the system drive", LIVE_WALK, || {
        (!handle.is_done()).then(|| {
            let p = handle.progress();
            p.files + p.dirs
        })
    });
    let walker = handle.wait();
    assert_sound(&walker);
    // The reader sees the disk, not the file system's cache. What was written
    // since the first flush is written out now, so that the table the reader
    // takes is not older than the walk it is compared with.
    phase("flush the system drive again", FLUSH_LIMIT, move || {
        flush_volume(letter)
    });
    let raw = PathBuf::from(format!(r"\\.\{drive}"));
    let read_root = canon.clone();
    let (reader, report) = phase("read the system drive's table", READ_LIMIT, move || {
        read_volume(&raw, &read_root, Accounting::Listing, None)
    });
    println!("{report:#?}");
    println!(
        "walker {} nodes, reader {} nodes",
        walker.snapshot.len(),
        reader.snapshot.len()
    );
    // What the table itself holds for the entries it marks as reparse points,
    // to read beside the system's answers for the ones the listing lacks.
    println!(
        "reparse points in the reader's tree, by tag in the table: {:?} (tags past the limit: {})",
        report
            .reparse
            .by_tag
            .iter()
            .map(|(t, n)| (format!("{t:#010x}"), *n))
            .collect::<Vec<_>>(),
        report.reparse.other
    );

    // The same table counted in full, every stream: the bytes of a file that
    // the overlay layer compressed are in a named stream the listing-style
    // count leaves out, and the full count is what the application shows.
    let raw = PathBuf::from(format!(r"\\.\{drive}"));
    let volume_device = raw.clone();
    let read_root = canon.clone();
    let (full, full_report) = phase(
        "read the system drive's table (full)",
        READ_LIMIT,
        move || read_volume(&raw, &read_root, Accounting::Full, None),
    );
    println!(
        "full count: {} nodes, extras {:?}",
        full.snapshot.len(),
        full_report.extras
    );

    // Sorting the differences asks the system about each path, so it can
    // take a while on a large drive.
    let (mut outcome, audit) = phase("sort the differences", ASK_LIMIT, {
        let (r, w, f) = (
            reader.snapshot.clone(),
            walker.snapshot.clone(),
            full.snapshot.clone(),
        );
        let oracle = SystemOracle::new(&canon).with_volume(&volume_device);
        let geo = live::Geometry {
            cluster_bytes: u64::from(full_report.cluster_bytes),
            record_bytes: u64::from(full_report.record_bytes),
        };
        move || {
            let outcome = live::classify(&r, &w, window_from, &oracle, Some(&f));
            let audit =
                live::audit_overlay_files(&flatten(&r), &flatten(&w), &flatten(&f), &oracle, geo);
            (outcome, audit)
        }
    });
    live::report_audit(&audit);
    // A file whose compressed stream is held inside its own table record is
    // explained by what was found for that very file.
    for (d, evidence) in audit.resident {
        let verdict = live::Verdict {
            class: live::C_RESIDENT_STREAM,
            basis: live::B_RESIDENT_STREAM,
            detail: evidence,
        };
        outcome.explained.push((d, verdict));
    }
    live::report(&outcome);
    // A file the overlay layer backs whose bytes the full count does not
    // match, for no reason that holds for it, is an under- or over-count by
    // the reader: not explained.
    for d in audit.differ {
        let why = "the full count is not the system's allocation for this file".to_owned();
        outcome.unexplained.push((d, why));
    }
    println!("unexplained after the audit: {}", outcome.unexplained.len());
    if strict {
        assert_eq!(
            outcome.unexplained.len(),
            0,
            "differences that no evidence explains (BYTESCAPE_NTFS_LIVE=report lists them without failing)"
        );
    }
    assert_eq!(report.lost_records, 0);
    assert_eq!(report.diagnostics.total(), 0, "{:?}", report.diagnostics);
    // No file may be without a place: one that no directory reaches is a
    // finding, not live change (the walker has no such thing). What the
    // reader leaves out on purpose (the reserved records, and anything below
    // a link or mount point that neither tool enters) is printed.
    assert_eq!(report.adopted.files, 0, "files that no directory reaches");
    println!("left out on purpose: {:?}", report.unreachable);
}

#[test]
fn a_volume_that_does_not_exist_is_an_error_not_a_panic() {
    let scanner = Arc::new(NtfsScanner::new(
        r"\\.\NoSuchVolume",
        NtfsOptions::default(),
    ));
    let result = start(scanner, PathBuf::from(r"Z:\"), ScanOptions::default()).wait();
    assert!(result.stats.error.is_some());
    assert!(!result.snapshot.scan_complete());
}

#[test]
fn a_file_that_is_not_ntfs_is_an_error_not_a_panic() {
    let path = std::env::temp_dir().join(format!("not-ntfs-{}.bin", std::process::id()));
    std::fs::write(&path, vec![0x5Au8; 1 << 16]).expect("write");
    let scanner = Arc::new(NtfsScanner::new(&path, NtfsOptions::default()));
    let result = start(scanner, PathBuf::from(r"X:\"), ScanOptions::default()).wait();
    assert!(result.stats.error.is_some());
    std::fs::remove_file(&path).ok();
}
