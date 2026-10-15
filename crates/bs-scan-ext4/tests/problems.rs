//! Typed problems: damage planted at known places in a real image, and what
//! the reader says about each. Each case checks the tree, the report and the
//! kind of problem, not just that nothing panicked.
//!
//! Needs `mke2fs` and `debugfs`.

// The reader and the tools these tests drive are Linux-only.
#![cfg(target_os = "linux")]

mod common;

use bs_scan::{ScanContext, ScanEnd, ScanOptions};
use bs_scan_ext4::{Ext4Error, Ext4Options, ProblemKind, read_filesystem};
use bs_tree::{NodeFlags, NodeKind, ReadErrorKind};
use common::*;
use std::fs;
use std::process::Command;

fn base_items() -> Vec<Item> {
    vec![
        dir("a"),
        dir("a/b"),
        file("a/one.txt", 1000),
        file("a/two.txt", 2000),
        file("a/b/deep.txt", 3000),
        dir("c"),
        file("c/three.txt", 5000),
        file("top.txt", 10),
    ]
}

struct Image {
    fixture: Fixture,
    bytes: Vec<u8>,
    block_size: u64,
}

impl Image {
    fn new(name: &str) -> Image {
        let fixture = build(name, &ImageSpec::new(base_items()).size("8M"));
        let bytes = fs::read(&fixture.img).unwrap();
        Image {
            fixture,
            bytes,
            block_size: 4096,
        }
    }

    /// Byte offset of an inode's record, from debugfs.
    fn inode_at(&self, path: &str) -> usize {
        let out = run(Command::new("debugfs")
            .args(["-R", &format!("imap {path}")])
            .arg(&self.fixture.img));
        let block: u64 = out
            .split("located at block ")
            .nth(1)
            .and_then(|s| s.split(',').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| panic!("no block in {out}"));
        let offset = out
            .split("offset 0x")
            .nth(1)
            .and_then(|s| u64::from_str_radix(s.split_whitespace().next().unwrap_or("0"), 16).ok())
            .unwrap();
        usize::try_from(block * self.block_size + offset).unwrap()
    }

    fn inode_number(&self, path: &str) -> u32 {
        let out = run(Command::new("debugfs")
            .args(["-R", &format!("stat {path}")])
            .arg(&self.fixture.img));
        out.split("Inode: ")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap()
    }

    fn first_block(&self, path: &str) -> usize {
        let out = run(Command::new("debugfs")
            .args(["-R", &format!("blocks {path}")])
            .arg(&self.fixture.img));
        let n: u64 = out
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap();
        usize::try_from(n * self.block_size).unwrap()
    }

    /// Offset of the directory entry called `name` in `dir`'s first block.
    fn dirent(&self, dir: &str, name: &str) -> usize {
        let start = self.first_block(dir);
        let mut at = start;
        loop {
            let rec = usize::from(u16::from_le_bytes([self.bytes[at + 4], self.bytes[at + 5]]));
            let len = usize::from(self.bytes[at + 6]);
            if self.bytes[at + 8..at + 8 + len] == *name.as_bytes() {
                return at;
            }
            assert!(
                rec >= 8 && at + rec < start + 4096,
                "no entry {name} in {dir}"
            );
            at += rec;
        }
    }

    fn put32(&mut self, at: usize, v: u32) {
        self.bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn put16(&mut self, at: usize, v: u16) {
        self.bytes[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// In-use inodes that nothing names on this image before any damage.
    fn orphans(&self) -> u64 {
        orphan_file_inodes(&self.bytes)
    }

    fn scan(&self) -> ImageScan {
        let path = self.fixture.root.join("damaged.img");
        fs::write(&path, &self.bytes).unwrap();
        scan_image(&path)
    }
}

fn names(scan: &ImageScan) -> Vec<String> {
    flatten(&scan.result.snapshot)
        .keys()
        .map(|k| lossy(k))
        .collect()
}

#[test]
fn an_undamaged_image_has_no_problems_and_every_inode_accounted_for() {
    let img = Image::new("clean");
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(report.diagnostics.total(), 0);
    assert_eq!(report.vanished_entries + report.unreadable_entries, 0);
    // Root + a, a/b, c + five files; lost+found is a directory too.
    assert_eq!(scan.result.snapshot.len(), 1 + 4 + 5);
    // The journal and the resize inode are reserved; the orphan file is
    // an in-use inode nothing names.
    assert!(report.reserved.inodes >= 1 && report.reserved.allocated > 0);
    assert_eq!(report.unreachable.inodes, img.orphans());
    assert!(!report.needs_recovery);
    assert!(report.metadata_csum);
    assert!(
        report.unverified_blocks > 0,
        "checksums are not verified, and say so"
    );
}

#[test]
fn an_entry_naming_a_free_inode_is_skipped_and_counted() {
    let mut img = Image::new("freeino");
    let at = img.dirent("/a", "one.txt");
    img.put32(at, 2000); // in range, never used
    let scan = img.scan();
    assert_sound(&scan.result);
    assert!(!names(&scan).contains(&"a/one.txt".to_string()));
    assert!(names(&scan).contains(&"a/two.txt".to_string()));
    let report = scan.report.clone().unwrap();
    assert_eq!(report.diagnostics.count(ProblemKind::DirentToFreeInode), 1);
    assert_eq!(report.vanished_entries, 1);
    // The file is still on the disk, in use, and now unreached.
    assert_eq!(report.unreachable.inodes, img.orphans() + 1);
    assert!(scan.result.snapshot.scan_complete());
}

#[test]
fn an_entry_naming_an_inode_of_another_type_is_skipped() {
    // As when an inode number was freed and handed out again.
    let mut img = Image::new("reused");
    let at = img.dirent("/a", "one.txt");
    let c = img.inode_number("/c");
    img.put32(at, c); // the entry still says "regular file"; the inode is a directory
    let scan = img.scan();
    assert_sound(&scan.result);
    assert!(!names(&scan).contains(&"a/one.txt".to_string()));
    let report = scan.report.clone().unwrap();
    assert_eq!(report.diagnostics.count(ProblemKind::DirentTypeMismatch), 1);
}

#[test]
fn an_entry_naming_an_inode_outside_the_file_system_is_skipped() {
    let mut img = Image::new("outside");
    let at = img.dirent("/a", "two.txt");
    img.put32(at, u32::MAX);
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(report.diagnostics.count(ProblemKind::DirentBadInode), 1);
    assert!(!names(&scan).contains(&"a/two.txt".to_string()));
    // Nothing says what the entry stood for, so its directory is marked.
    let infos = flatten(&scan.result.snapshot);
    assert!(infos[&b"a"[..]].failed);
    assert!(scan.result.snapshot.root().unwrap().has_unreadable_below());
    assert_eq!(report.unreadable_entries, 1);
}

#[test]
fn a_directory_entered_twice_is_entered_once() {
    // An entry in a/b that names its own grandparent's child, a: a loop.
    let mut img = Image::new("loop");
    let at = img.dirent("/a/b", "deep.txt");
    let a = img.inode_number("/a");
    img.put32(at, a);
    img.bytes[at + 7] = 2; // say "directory"
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(
        report.diagnostics.count(ProblemKind::DirectoryLinkedTwice),
        1
    );
    let infos = flatten(&scan.result.snapshot);
    // The tree holds `a` once, and is finite.
    assert_eq!(infos.keys().filter(|k| k.ends_with(b"a")).count(), 1);
    assert!(scan.result.snapshot.scan_complete());
}

/// A tree with a directory two levels deep, to lose in one piece.
fn lost_subtree_items() -> Vec<Item> {
    vec![
        dir("a"),
        file("a/one.txt", 1000),
        dir("c"),
        file("c/three.txt", 50_000),
        dir("c/sub"),
        file("c/sub/four.bin", 200_000),
    ]
}

fn lost_subtree_image(name: &str) -> Image {
    let fixture = build(name, &ImageSpec::new(lost_subtree_items()).size("8M"));
    let bytes = fs::read(&fixture.img).unwrap();
    Image {
        fixture,
        bytes,
        block_size: 4096,
    }
}

/// What must hold when the four inodes of `c` were dropped: they are gone
/// from the tree, their bytes are reported as unreached, and the directory
/// whose listing held the entry (the root) says it is incomplete, with every
/// total above it a lower bound.
fn assert_c_was_lost_loudly(img: &Image, scan: &ImageScan) {
    let report = scan.report.clone().unwrap();
    let infos = flatten(&scan.result.snapshot);
    assert!(!infos.contains_key(&b"c"[..]), "c must not be adopted");
    assert!(!infos.contains_key(&b"c/three.txt"[..]));
    assert!(infos.contains_key(&b"a/one.txt"[..]));
    let root = scan.result.snapshot.root().unwrap();
    assert!(
        root.is_unreadable(),
        "the listing that lost c is not complete"
    );
    assert_eq!(
        root.read_failure().map(|f| f.kind),
        Some(ReadErrorKind::Corrupt)
    );
    assert!(
        root.bytes_are_lower_bound(),
        "totals must carry the lower bound"
    );
    assert!(report.unreadable_entries >= 1);
    assert_eq!(report.unreachable.inodes, img.orphans() + 4);
    assert!(
        report.unreachable.allocated >= 250_000,
        "{:?}",
        report.unreachable
    );
    // `a` was listed whole and stays complete.
    assert!(!infos[&b"a"[..]].failed);
}

#[test]
fn a_directory_that_belongs_elsewhere_is_not_adopted_and_its_listing_says_so() {
    // `c` is named only by the root, and its own `..` names `a`. Nothing else
    // leads there, so the mismatch is the one thing that decides its fate.
    let mut img = lost_subtree_image("adopt");
    let a = img.inode_number("/a");
    let c0 = img.first_block("/c");
    // `..` is the second entry of the block, 12 bytes in.
    assert_eq!(&img.bytes[c0 + 12 + 8..c0 + 12 + 10], b"..");
    img.put32(c0 + 12, a);
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(
        report
            .diagnostics
            .count(ProblemKind::DirectoryParentMismatch),
        1,
        "{:?}",
        report.diagnostics
    );
    assert_eq!(
        report.diagnostics.count(ProblemKind::DirectoryLinkedTwice),
        0
    );
    assert_c_was_lost_loudly(&img, &scan);
}

#[test]
fn an_entry_whose_type_byte_is_wrong_loses_a_subtree_loudly() {
    // The root's entry for `c` says "regular file"; the inode is a directory.
    let mut img = lost_subtree_image("typeflip");
    let at = img.dirent("/", "c");
    assert_eq!(img.bytes[at + 7], 2);
    img.bytes[at + 7] = 1;
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(report.diagnostics.count(ProblemKind::DirentTypeMismatch), 1);
    assert_c_was_lost_loudly(&img, &scan);
}

#[test]
fn an_entry_naming_a_reserved_inode_marks_its_listing() {
    // Inode 8 is the journal: in use, reserved, and in no directory.
    let mut img = Image::new("reserved");
    let at = img.dirent("/a", "two.txt");
    img.put32(at, 8);
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(
        report.diagnostics.count(ProblemKind::DirentToReservedInode),
        1
    );
    let infos = flatten(&scan.result.snapshot);
    assert!(infos[&b"a"[..]].failed);
    assert!(!names(&scan).contains(&"a/two.txt".to_string()));
}

#[test]
fn a_dropped_entry_for_an_inode_that_was_already_counted_loses_nothing() {
    // /a's entry "two.txt" now names top.txt's inode but calls it a
    // directory. The root has been listed whole by then, so top.txt is in the
    // tree: the entry is skipped, and nothing is missing.
    let mut img = Image::new("counted");
    let top = img.inode_number("/top.txt");
    let at = img.dirent("/a", "two.txt");
    img.put32(at, top);
    img.bytes[at + 7] = 2;
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(report.diagnostics.count(ProblemKind::DirentTypeMismatch), 1);
    let infos = flatten(&scan.result.snapshot);
    assert!(
        !infos[&b"a"[..]].failed,
        "nothing was lost, so nothing is flagged"
    );
    assert!(infos.contains_key(&b"top.txt"[..]));
    assert!(!scan.result.snapshot.root().unwrap().has_unreadable_below());
    assert_eq!(report.unreadable_entries, 0);
}

#[test]
fn an_orphan_file_is_reported_as_unreachable() {
    // Asked for explicitly, whatever the host's mke2fs turns on by default.
    let f = build(
        "orphanfile",
        &ImageSpec::new(base_items())
            .args(&["-O", "orphan_file"])
            .size("8M"),
    );
    let bytes = fs::read(&f.img).unwrap();
    assert_eq!(
        orphan_file_inodes(&bytes),
        1,
        "the image must have an orphan file"
    );
    let scan = scan_image(&f.img);
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(report.diagnostics.total(), 0);
    assert_eq!(report.unreachable.inodes, 1);
}

#[test]
fn a_directory_block_with_a_bad_length_is_unreadable_and_keeps_what_came_before() {
    let mut img = Image::new("badlen");
    // Break the entry after "one.txt" in /a, whatever comes first.
    let first = img.dirent("/a", "one.txt");
    let rec = usize::from(u16::from_le_bytes([
        img.bytes[first + 4],
        img.bytes[first + 5],
    ]));
    img.put16(first + rec + 4, 3); // not a multiple of four, and too short
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(
        report.diagnostics.count(ProblemKind::DirectoryBlockCorrupt),
        1
    );
    let infos = flatten(&scan.result.snapshot);
    let a = &infos[&b"a"[..]];
    assert!(a.failed, "the directory must be marked unreadable");
    assert!(
        infos.contains_key(&b"a/one.txt"[..]),
        "entries before the damage stay"
    );
    // The listing is incomplete, so nothing above it claims to be complete.
    assert!(scan.result.snapshot.root().unwrap().has_unreadable_below());
    assert_eq!(report.unreadable_entries, 1);
}

#[test]
fn an_unreadable_inode_table_marks_the_entries_it_held() {
    let mut img = Image::new("noinode");
    // An inode with a type that does not exist.
    let at = img.inode_at("/a/two.txt");
    img.put16(at, 0x3000 | 0o644);
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(report.diagnostics.count(ProblemKind::InodeInvalid), 1);
    let infos = flatten(&scan.result.snapshot);
    let two = &infos[&b"a/two.txt"[..]];
    assert!(two.failed);
    assert_eq!((two.logical, two.allocated), (0, 0));
    assert_eq!(
        two.kind,
        NodeKind::File,
        "the listing still says what it is"
    );
}

#[test]
fn a_file_with_no_name_is_reported_as_unreachable_bytes() {
    let mut img = Image::new("orphan");
    let at = img.dirent("/a", "two.txt");
    img.put32(at, 0); // the name is gone, the inode remains
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(report.unreachable.inodes, img.orphans() + 1);
    // The unnamed file (one block), and the orphan file (128 KiB here) if there is one.
    assert!(report.unreachable.allocated >= 4096 + img.orphans() * 131_072);
    assert!(!names(&scan).contains(&"a/two.txt".to_string()));
}

#[test]
fn a_destroyed_root_is_a_typed_error() {
    let mut img = Image::new("noroot");
    let at = img.inode_at("/");
    for b in &mut img.bytes[at..at + 256] {
        *b = 0;
    }
    let scan = img.scan();
    assert!(scan.report.is_none());
    assert!(scan.result.stats.error.is_some());
    // Whatever the scanner did, the tree it hands back is valid and says it
    // is incomplete.
    scan.result.snapshot.verify().unwrap();
    assert!(!scan.result.snapshot.scan_complete());
}

#[test]
fn files_that_are_not_ext4_or_not_supported_are_refused_by_type() {
    let img = Image::new("refuse");
    let (ctx, _rx) = ScanContext::channel(ScanOptions::default(), 1 << 12);
    let run_on = |bytes: &[u8]| {
        let v = bytes.to_vec();
        read_filesystem(&v, b"/", &Ext4Options::default(), &ctx).map(|(e, _)| e)
    };
    assert!(matches!(run_on(&[0u8; 4096]), Err(Ext4Error::NotExt4)));
    assert!(matches!(run_on(&[]), Err(Ext4Error::Io(_))));
    let mut compressed = img.bytes.clone();
    compressed[1024 + 0x60] |= 0x01;
    assert!(matches!(
        run_on(&compressed),
        Err(Ext4Error::Unsupported(_))
    ));
    let mut huge = img.bytes.clone();
    huge[1024 + 4..1024 + 8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(run_on(&huge), Err(Ext4Error::BadSuperblock(_))));
}

#[test]
fn a_memory_limit_refuses_before_allocating() {
    let img = Image::new("limit");
    let (ctx, _rx) = ScanContext::channel(ScanOptions::default(), 1 << 12);
    let options = Ext4Options {
        max_memory_bytes: 100,
        ..Ext4Options::default()
    };
    let r = read_filesystem(&img.bytes, b"/", &options, &ctx);
    assert!(matches!(r, Err(Ext4Error::TooLarge(_))), "{r:?}");
}

#[test]
fn the_directory_list_is_charged_to_the_memory_limit() {
    let img = Image::new("pending");
    let (ctx, _rx) = ScanContext::channel(ScanOptions::default(), 1 << 12);
    let try_limit = |limit: u64| {
        let options = Ext4Options {
            max_memory_bytes: limit,
            ..Ext4Options::default()
        };
        read_filesystem(&img.bytes, b"/", &options, &ctx).map(|(_, report)| report)
    };
    // The smallest limit that lets the read through.
    let (mut low, mut high) = (0u64, 1u64 << 24);
    assert!(try_limit(high).is_ok());
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if try_limit(mid).is_ok() {
            high = mid;
        } else {
            low = mid;
        }
    }
    let report = try_limit(high).unwrap();
    assert_eq!(report.memory_cap_bytes, high);
    // Just below it, the read is refused. Somewhere in the window beneath it
    // the inode records fit and the directories do not: that is the list of
    // directories being counted.
    let mut refused_for_directories = false;
    for limit in (high.saturating_sub(4096)..high).step_by(4) {
        if let Err(Ext4Error::TooLarge(why)) = try_limit(limit)
            && why.contains("directories exceed")
        {
            refused_for_directories = true;
        }
    }
    assert!(refused_for_directories);
}

#[test]
fn a_mount_that_shows_part_of_the_file_system_is_refused() {
    let img = Image::new("partial");
    let (ctx, _rx) = ScanContext::channel(ScanOptions::default(), 1 << 12);
    for root in [&b"/sub"[..], b"", b"//", b"/.."] {
        let options = Ext4Options {
            mount_root: Some(root.to_vec()),
            ..Ext4Options::default()
        };
        let r = read_filesystem(&img.bytes, b"/", &options, &ctx);
        assert!(matches!(r, Err(Ext4Error::PartialMount)), "{root:?}: {r:?}");
    }
    for root in [Some(b"/".to_vec()), None] {
        let options = Ext4Options {
            mount_root: root.clone(),
            ..Ext4Options::default()
        };
        let r = read_filesystem(&img.bytes, b"/", &options, &ctx);
        assert!(r.is_ok(), "{root:?}: {r:?}");
    }
    // Through the scanner interface the refusal is an error of the scan, and
    // the tree stays empty and incomplete.
    let path = img.fixture.root.join("partial.img");
    fs::write(&path, &img.bytes).unwrap();
    let options = Ext4Options {
        mount_root: Some(b"/data".to_vec()),
        ..Ext4Options::default()
    };
    let scan = scan_image_with(&path, options, ScanOptions::default());
    assert!(scan.result.stats.error.is_some());
    assert!(!scan.result.snapshot.scan_complete());
}

#[test]
fn cancelling_keeps_a_valid_incomplete_tree() {
    let img = Image::new("cancel");
    // Cancelled before the read starts.
    let (ctx, rx) = ScanContext::channel(ScanOptions::default(), 1 << 12);
    ctx.cancel();
    let (end, report) = read_filesystem(&img.bytes, b"/", &Ext4Options::default(), &ctx).unwrap();
    assert_eq!(end, ScanEnd::Cancelled);
    assert!(report.cancelled);
    assert!(rx.try_recv().is_err(), "nothing was sent");

    // The receiver goes away after the first batch.
    let (ctx, rx) = ScanContext::channel(ScanOptions::default(), 1);
    drop(rx);
    let (end, report) = read_filesystem(&img.bytes, b"/", &Ext4Options::default(), &ctx).unwrap();
    assert_eq!(end, ScanEnd::Cancelled);
    assert!(report.cancelled);

    // Through the scanner interface: a stopped scan is flagged incomplete.
    let path = img.fixture.root.join("c.img");
    fs::write(&path, &img.bytes).unwrap();
    let handle = bs_scan::start(
        std::sync::Arc::new(bs_scan_ext4::Ext4Scanner::new(
            &path,
            Ext4Options::default(),
        )),
        "/".into(),
        ScanOptions::default(),
    );
    handle.cancel();
    let result = handle.wait();
    result.snapshot.verify().unwrap();
    if result.stats.end == Some(ScanEnd::Cancelled) {
        assert!(!result.snapshot.scan_complete());
        assert!(result.snapshot.scan_stopped());
    }
}

#[test]
fn symlinks_are_never_followed_and_hard_links_flag_both_names() {
    let items = vec![
        file("real", 5000),
        symlink_to("link", "real"),
        link("alias", "real"),
        dir("d"),
        symlink_to("d/up", ".."),
    ];
    let f = build("links2", &ImageSpec::new(items));
    let scan = scan_image(&f.img);
    assert_sound(&scan.result);
    let infos = flatten(&scan.result.snapshot);
    let l = &infos[&b"link"[..]];
    assert_eq!(l.kind, NodeKind::Symlink);
    assert!(l.flags.contains(NodeFlags::NOT_FOLLOWED));
    assert_eq!(l.logical, 4, "the link's own length");
    let (a, r) = (&infos[&b"alias"[..]], &infos[&b"real"[..]]);
    assert!(a.flags.contains(NodeFlags::HARD_LINK) && r.flags.contains(NodeFlags::HARD_LINK));
    // Bytes counted once.
    assert_eq!(a.allocated + r.allocated, 8192);
    assert!(scan.result.snapshot.scan_complete());
}

#[test]
fn a_long_chain_of_directories_needs_no_recursion() {
    // A chain of 200 levels, built by hand.
    let mut items = Vec::new();
    let mut p = String::from("x");
    items.push(dir(&p));
    for _ in 0..200 {
        p.push_str("/x");
        items.push(dir(&p));
    }
    let f = build("chain", &ImageSpec::new(items));
    let scan = scan_image(&f.img);
    assert_sound(&scan.result);
    assert_eq!(scan.result.snapshot.len(), 1 + 201 + 1);
}

#[test]
fn an_inline_directory_continues_in_its_attribute() {
    // mke2fs never spills an inline directory into its attribute, but the
    // kernel does when names are added to a mounted one. Plant that layout:
    // the entries of `pool` are moved into the `system.data` attribute of
    // `four`, an inline directory that already holds four names.
    let mut items = vec![
        dir("four"),
        dir("pool"),
        file("pool/p1", 7000),
        file("pool/p2", 100),
    ];
    // Enough names that `pool` itself needs a block.
    for n in 3..8 {
        items.push(file(&format!("pool/p{n}"), 10));
    }
    for name in ["a1", "a2", "a3", "a4"] {
        items.push(file(&format!("four/{name}"), 3));
    }
    let fixture = build(
        "inlinetail",
        &ImageSpec::new(items)
            .args(&["-O", "inline_data", "-I", "256"])
            .size("8M"),
    );
    let bytes = fs::read(&fixture.img).unwrap();
    let mut img = Image {
        fixture,
        bytes,
        block_size: 4096,
    };
    let p1 = img.inode_number("/pool/p1");
    let p2 = img.inode_number("/pool/p2");
    let four = img.inode_at("/four");
    // Remove the names from `pool`.
    for name in ["p1", "p2"] {
        let at = img.dirent("/pool", name);
        img.put32(at, 0);
    }
    // The attribute area starts at 128 + extra size (32): magic, then the
    // one entry `system.data`.
    let ea = four + 160;
    assert_eq!(
        &img.bytes[ea..ea + 4],
        &[0, 0, 0x02, 0xEA],
        "expected an attribute area"
    );
    let entry = ea + 4;
    assert_eq!(img.bytes[entry], 4);
    assert_eq!(img.bytes[entry + 1], 7);
    // Value: two entries of 12 bytes, placed after the entry and its end mark.
    let value_offset = 40usize;
    img.put16(entry + 2, value_offset as u16);
    img.put32(entry + 8, 24);
    let value = entry + value_offset;
    for (i, (ino, name)) in [(p1, b"x1"), (p2, b"x2")].into_iter().enumerate() {
        let at = value + i * 12;
        img.put32(at, ino);
        img.put16(at + 4, 12);
        img.bytes[at + 6] = 2;
        img.bytes[at + 7] = 1;
        img.bytes[at + 8..at + 10].copy_from_slice(name);
        img.bytes[at + 10..at + 12].copy_from_slice(&[0, 0]);
    }
    img.put32(four + 4, 60 + 24);
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert_eq!(report.diagnostics.total(), 0, "{:?}", report.diagnostics);
    let infos = flatten(&scan.result.snapshot);
    for name in ["a1", "a2", "a3", "a4", "x1", "x2"] {
        assert!(
            infos.contains_key(format!("four/{name}").as_bytes()),
            "four/{name}"
        );
    }
    assert_eq!(infos[&b"four/x1"[..]].logical, 7000);
    assert_eq!(infos[&b"four/x2"[..]].logical, 100);
    assert!(!infos.contains_key(&b"pool/p1"[..]) && !infos.contains_key(&b"pool/p2"[..]));
    assert!(infos.contains_key(&b"pool/p3"[..]));
    assert_eq!(
        report.unreachable.inodes,
        img.orphans(),
        "only the orphan file, if the image has one, is unreached"
    );
}

#[test]
fn directories_sharing_blocks_cannot_make_the_reader_read_more_than_the_disk_holds() {
    // Three directories claim the same 1000 blocks of a 2048-block file
    // system. A consistent file system has each directory block in one
    // directory, so reading them all would take 3000 block reads from a disk
    // that has 2048 blocks: the third claim is refused.
    let mut img = Image::new("shared");
    for path in ["/a", "/c", "/a/b"] {
        let at = img.inode_at(path);
        let root = at + 0x28;
        img.bytes[root..root + 60].fill(0);
        img.put16(root, 0xF30A); // magic
        img.put16(root + 2, 1); // one entry
        img.put16(root + 4, 4); // room for four
        img.put16(root + 12 + 4, 1000); // length
        img.put32(root + 12 + 8, 500); // first block
        img.put32(at + 4, 1000 * 4096); // size
    }
    let scan = img.scan();
    assert_sound(&scan.result);
    let report = scan.report.clone().unwrap();
    assert!(
        report.diagnostics.count(ProblemKind::ReadBudgetExceeded) >= 1,
        "{:?}",
        report.diagnostics
    );
    // The directories that could not be read are marked, the rest is intact.
    let infos = flatten(&scan.result.snapshot);
    assert_eq!(infos[&b"top.txt"[..]].logical, 10);
    assert!(scan.result.snapshot.root().unwrap().has_unreadable_below());
    assert!(scan.result.snapshot.scan_complete());
}
