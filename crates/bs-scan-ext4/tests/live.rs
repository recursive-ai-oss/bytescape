//! The raw reader against the walker on the machine's own root file system.
//!
//! This is the check that matters most: a live, mounted, changing ext4 rather
//! than a quiet image. It reads the block device under `/` and scans `/` with
//! the walker, then compares them path by path. Files that change while the
//! test runs are found by scanning with the walker twice, before and after
//! the reader, and are set apart; every other difference is a finding.
//!
//! It needs root (to read the device) and a disposable machine, because it
//! reads the whole root device:
//!
//! ```text
//! BYTESCAPE_DISPOSABLE_HOST=1 sudo -E target/debug/deps/live-… --nocapture
//! ```
//!
//! Without `BYTESCAPE_DISPOSABLE_HOST=1` the test prints `NOT RUN` and passes,
//! so that the privileged CI step, which fails on that text, notices. With it
//! set, a machine that cannot run the test makes the test fail.

// The reader and the tools these tests drive are Linux-only.
#![cfg(target_os = "linux")]

mod common;

use bs_scan::{ScanOptions, Walker};
use bs_scan_ext4::Ext4Options;
use bs_tree::NodeFlags;
use common::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, PartialEq, Eq)]
struct Mount {
    device: String,
    mount_point: Vec<u8>,
    fs_type: String,
}

/// Undoes the `\040`-style escapes the kernel puts in mount point names.
fn unescape(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && let Some(code) = text
                .get(i + 1..i + 4)
                .and_then(|o| u8::from_str_radix(o, 8).ok())
        {
            out.push(code);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let fields: Vec<&str> = before.split(' ').collect();
            let fs_type = after.split(' ').next()?.to_owned();
            Some(Mount {
                device: (*fields.get(2)?).to_owned(),
                mount_point: unescape(fields.get(4)?),
                fs_type,
            })
        })
        .collect()
}

/// The mount points below `/` other than `/` itself, as paths relative to it,
/// each once.
fn mounts_below_root(mounts: &[Mount]) -> Vec<Vec<u8>> {
    let mut seen = BTreeSet::new();
    for m in mounts {
        if let Some(rest) = m.mount_point.strip_prefix(b"/")
            && !rest.is_empty()
        {
            seen.insert(rest.to_vec());
        }
    }
    seen.into_iter().collect()
}

fn root_mount(mounts: &[Mount]) -> Option<&Mount> {
    // The last mount on `/` is the visible one.
    mounts.iter().rev().find(|m| m.mount_point == b"/")
}

#[test]
fn mountinfo_lines_are_understood() {
    let text = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n\
23 22 0:5 / /proc rw - proc proc rw\n\
24 22 8:2 / /mnt/with\\040space rw - ext4 /dev/sda2 rw\n\
25 22 8:1 /var/lib /srv/bind rw - ext4 /dev/sda1 rw\n\
26 22 0:30 / / rw - overlay overlay rw\n";
    let mounts = parse_mountinfo(text);
    assert_eq!(mounts.len(), 5);
    assert_eq!(mounts[0].device, "8:1");
    assert_eq!(mounts[2].mount_point, b"/mnt/with space");
    assert_eq!(
        mounts_below_root(&mounts),
        [
            b"mnt/with space".to_vec(),
            b"proc".to_vec(),
            b"srv/bind".to_vec()
        ]
    );
    assert_eq!(root_mount(&mounts).unwrap().fs_type, "overlay");
    assert_eq!(unescape("a\\134b"), b"a\\b");
    assert_eq!(unescape("trailing\\"), b"trailing\\");
}

/// What the comparison found.
struct Outcome {
    /// Counts by (field, class).
    classes: BTreeMap<(&'static str, &'static str), u64>,
    /// Differences on paths that were stable, with no known reason.
    findings: Vec<Diff>,
}

/// Reads `device` with the raw reader and `root` with the walker, twice, and
/// sorts the differences into those on paths that changed between the two
/// walks, those at mount points, and the rest.
fn compare_live(device: &Path, root: &Path, skip: Vec<Vec<u8>>, image_only: &[&[u8]]) -> Outcome {
    // Let delayed allocation settle, so the walker's block counts are final.
    let _ = Command::new("sync").status();
    let first = Walker::scan_blocking(root, ScanOptions::default());
    let reader = scan_image_with(
        device,
        Ext4Options {
            skip,
            ..Ext4Options::default()
        },
        ScanOptions::default(),
    );
    let second = Walker::scan_blocking(root, ScanOptions::default());
    assert!(
        reader.result.stats.error.is_none(),
        "the reader failed: {:?}",
        reader.result.stats.error
    );
    reader.result.snapshot.verify().expect("tree invariants");
    let report = reader.report.expect("a report");
    eprintln!(
        "reader: {} entries, load {:?}, emit {:?}; walker: {} then {} entries",
        reader.result.snapshot.len(),
        report.load_time,
        report.emit_time,
        first.snapshot.len(),
        second.snapshot.len()
    );
    eprintln!(
        "problems: {} {:?}; vanished {}; unreadable {}; unreachable inodes {} ({} B);          reserved inodes {} ({} B); hidden under mounts {:?}",
        report.diagnostics.total(),
        report.diagnostics.nonzero().collect::<Vec<_>>(),
        report.vanished_entries,
        report.unreadable_entries,
        report.unreachable.inodes,
        report.unreachable.allocated,
        report.reserved.inodes,
        report.reserved.allocated,
        report.hidden_under_mounts
    );

    // Paths the two walker scans disagree about changed while the test ran.
    let unstable: BTreeSet<String> = compare_trees(&second.snapshot, &first.snapshot, &[])
        .into_iter()
        .map(|d| d.path)
        .collect();
    let diffs = compare_trees(&reader.result.snapshot, &first.snapshot, image_only);
    let infos = flatten(&reader.result.snapshot);

    let mut classes = BTreeMap::new();
    let mut findings = Vec::new();
    for d in diffs {
        if unstable.contains(&d.path) {
            *classes
                .entry((d.field, "changed during the run"))
                .or_default() += 1;
            continue;
        }
        // A mount point: the walker's numbers describe the file system
        // mounted there; the reader sees the directory under it.
        if d.field == "mtime"
            && infos
                .get(d.path.as_bytes())
                .is_some_and(|i| i.flags.contains(NodeFlags::NOT_FOLLOWED))
        {
            *classes.entry((d.field, "mount point")).or_default() += 1;
            continue;
        }
        *classes.entry((d.field, "unexplained")).or_default() += 1;
        findings.push(d);
    }
    eprintln!("differences by class:");
    for ((field, class), n) in &classes {
        eprintln!("  {field:<18} {class:<24} {n}");
    }
    Outcome { classes, findings }
}

#[test]
fn the_comparison_sorts_differences_on_a_quiet_image() {
    // The same code the live test runs, on an image and the tree it was made
    // from, which does not change: nothing may be set apart as changed, and
    // the only unexplained differences are the directories' own blocks and
    // the image-only lost+found.
    let items = vec![
        dir("d"),
        dir("mnt"),
        dir("mnt/other"),
        file("mnt/other/hidden", 100),
        file("d/a", 5000),
        file("d/b", 70),
        link("d/c", "d/a"),
    ];
    let f = build("livelogic", &ImageSpec::new(items));
    let out = compare_live(
        &f.img,
        &f.src,
        vec![b"mnt/other".to_vec()],
        &[b"lost+found"],
    );
    assert!(
        out.classes
            .keys()
            .all(|(_, class)| *class != "changed during the run")
    );
    // The walker saw the contents of mnt/other, the reader was told not to:
    // the entries below it are the only presence differences.
    let presence: Vec<&str> = out
        .findings
        .iter()
        .filter(|d| d.field == "presence")
        .map(|d| d.path.as_str())
        .collect();
    assert_eq!(presence, ["mnt/other/hidden"]);
    assert!(
        out.findings
            .iter()
            .all(|d| d.field == "presence" || d.field == "allocated" || d.field == "flags"),
        "{:?}",
        out.findings
    );
}

#[test]
fn live_root_file_system_equals_the_walker() {
    if std::env::var_os("BYTESCAPE_DISPOSABLE_HOST").is_none_or(|v| v != "1") {
        eprintln!(
            "NOT RUN: the live comparison reads the root device as root;              set BYTESCAPE_DISPOSABLE_HOST=1 on a disposable machine"
        );
        return;
    }
    let info = std::fs::read_to_string("/proc/self/mountinfo").expect("mountinfo");
    let mounts = parse_mountinfo(&info);
    let root = root_mount(&mounts).expect("a mount on /");
    if root.fs_type != "ext4" {
        eprintln!(
            "NOT RUN: the root file system here is {}, not ext4",
            root.fs_type
        );
        return;
    }
    let device = PathBuf::from(format!("/dev/block/{}", root.device));
    assert!(
        std::fs::File::open(&device).is_ok(),
        "cannot read {} (the live test needs root)",
        device.display()
    );
    let skip = mounts_below_root(&mounts);
    eprintln!(
        "root device {} ({}), {} other mount points excluded",
        device.display(),
        root.device,
        skip.len()
    );
    let out = compare_live(&device, Path::new("/"), skip, &[]);
    assert_no_diffs("live: reader vs walker, stable paths", &out.findings);
}
