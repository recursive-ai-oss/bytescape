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
//! The system-drive comparison fails on any difference that live change does
//! not explain. `BYTESCAPE_NTFS_LIVE=report` prints the differences and does
//! not fail on them (for finding out what a new class of difference is); any
//! other value is the strict default. Every phase of both tests runs under a
//! time bound that names the phase.
mod common;

use bs_scan::{ScanOptions, ScanResult, Walker, start};
use bs_scan_ntfs::{Accounting, NtfsOptions, NtfsReport, NtfsScanner};
use bs_tree::{NodeFlags, NodeKind, Snapshot};
use common::compare::*;
use common::testkit::{Info, flatten};
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

const SYSTEM_NAMES: &[&str] = &[
    "$MFT", "$MFTMirr", "$LogFile", "$Volume", "$AttrDef", "$Bitmap", "$Boot", "$BadClus",
    "$Secure", "$UpCase", "$Extend",
];

fn first_part(path: &str) -> &str {
    path.split('/').next().unwrap_or("")
}

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

/// The fields a change made while the volume is read can explain: a size, a
/// modification time, a name that appeared or went. Never a flag or a kind: a
/// file does not become a link or a placeholder because it was written.
const LIVE_FIELDS: &[&str] = &[
    "mtime",
    "logical",
    "allocated",
    "presence",
    "link group",
    "link group bytes",
];

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

/// Whether the volume changed at this path inside the scan window: the entry
/// itself was written since `from`, or (for a name that appeared or went) the
/// directory that holds it was.
fn changed_in_window(
    d: &Diff,
    reader: &std::collections::BTreeMap<Vec<u8>, Info>,
    walker: &std::collections::BTreeMap<Vec<u8>, Info>,
    from: i64,
) -> bool {
    if !LIVE_FIELDS.contains(&d.field) {
        return false;
    }
    let at = |m: &std::collections::BTreeMap<Vec<u8>, Info>, p: &str| {
        m.get(p.as_bytes()).is_some_and(|i| i.mtime >= from)
    };
    let own = at(reader, &d.path) || at(walker, &d.path);
    let by_presence = d.field.starts_with("presence") || d.field == "link group";
    let dir = parent_of(&d.path);
    own || (by_presence && !dir.is_empty() && (at(reader, dir) || at(walker, dir)))
}

/// The machine's own drive is a volume that is changing while it is read.
/// Differences are sorted into the classes a live volume produces and
/// reported; a difference in none of them fails.
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
    let walked = canon.clone();
    let walker = phase("walk the system drive", WALK_LIMIT, move || {
        Walker::scan_blocking(&walked, ScanOptions::default())
    });
    assert_sound(&walker);
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

    let unexplained = phase("compare the trees", COMPARE_LIMIT, {
        let (r, w) = (reader.snapshot.clone(), walker.snapshot.clone());
        move || classify_live_differences(&r, &w, window_from)
    });
    println!("unexplained differences on the system drive: {unexplained}");
    if strict {
        assert_eq!(
            unexplained, 0,
            "differences that no live change explains (BYTESCAPE_NTFS_LIVE=report lists them without failing)"
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

/// Sorts every difference into its class, prints the classes, and returns how
/// many are in a class that no live change explains.
fn classify_live_differences(reader: &Snapshot, walker: &Snapshot, window_from: i64) -> usize {
    let diffs = compare_trees(reader, walker);
    let ra = flatten(reader);
    let wa = flatten(walker);
    let mut classes: std::collections::BTreeMap<&'static str, Vec<Diff>> = Default::default();
    for d in diffs {
        let key = d.path.as_bytes().to_vec();
        let w = wa.get(&key);
        let class = if d.field == "presence" && SYSTEM_NAMES.contains(&first_part(&d.path)) {
            "system files (reader only)"
        } else if d.field == "presence" && first_part(&d.path) == "System Volume Information" {
            "system volume information (walker refused)"
        } else if w.is_some_and(|i| i.failed) && !ra.contains_key(&key) {
            "walker could not read the directory"
        } else if changed_in_window(&d, &ra, &wa, window_from) {
            "changed during the scan (mtime, size, presence)"
        } else if d.field == "presence" {
            "presence (unexplained)"
        } else {
            "other (unexplained)"
        };
        classes.entry(class).or_default().push(d);
    }
    let mut unexplained = 0;
    for (class, list) in &classes {
        let mut by_field: std::collections::BTreeMap<&str, usize> = Default::default();
        for d in list {
            *by_field.entry(d.field).or_default() += 1;
        }
        println!("{class}: {} {by_field:?}", list.len());
        let mut shown: std::collections::BTreeMap<&str, usize> = Default::default();
        for d in list {
            let n = shown.entry(d.field).or_default();
            if *n < 12 {
                println!(
                    "    {:?} {}: reader {} / walker {}",
                    d.path, d.field, d.reader, d.other
                );
                *n += 1;
            }
        }
        if class.contains("unexplained") {
            unexplained += list.len();
        }
    }
    unexplained
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
