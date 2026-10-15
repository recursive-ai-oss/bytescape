//! The reader over real btrfs images: built with `mkfs.btrfs --rootdir` from a
//! directory tree and read without mounting, through a parser of the image's
//! B-trees that stands in for the kernel's tree search. No privileges are
//! needed; the tools (`mkfs.btrfs`) are, and the tests fail loudly without
//! them.
//!
//! What a real image proves that the in-memory tests cannot: that the item
//! layouts the reader decodes are the ones btrfs writes, and that a tree
//! `mkfs` built reads back with the sizes, times, names and links of the
//! directory it was made from.
#![cfg(all(target_os = "linux", feature = "image-tests"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use bs_scan_btrfs::{BtrfsOptions, Key, TreeSource};
use bs_tree::{NodeFlags, NodeKind};
use common::image::ImageSource;
use common::{START, try_read};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A scratch directory below `TMPDIR`, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("bs-btrfs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(cmd: &mut Command) {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("cannot run {cmd:?}: {e}"));
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// The fixture: every kind of thing the reader describes that `mkfs` can make
/// without mounting.
fn build_tree(root: &Path) {
    fs::create_dir_all(root.join("sub/deeper/deepest")).unwrap();
    fs::create_dir_all(root.join("empty-dir")).unwrap();
    fs::write(root.join("random.bin"), pseudo_random(300_000, 7)).unwrap();
    fs::write(
        root.join("sub/compressible.txt"),
        "compress me please\n".repeat(40_000),
    )
    .unwrap();
    fs::write(root.join("sub/deeper/small"), b"hi\n").unwrap();
    fs::write(root.join("sub/deeper/deepest/zero.bin"), vec![0u8; 1 << 20]).unwrap();
    fs::write(root.join("empty-file"), b"").unwrap();
    fs::hard_link(root.join("sub/deeper/small"), root.join("hard-a")).unwrap();
    fs::hard_link(root.join("sub/deeper/small"), root.join("sub/hard-b")).unwrap();
    fs::hard_link(root.join("random.bin"), root.join("empty-dir/random-again")).unwrap();
    symlink("sub/deeper/small", root.join("link")).unwrap();
    symlink("/nonexistent/target/that/is/long", root.join("dangling")).unwrap();
    let f = fs::File::create(root.join("sparse")).unwrap();
    f.set_len(3 << 20).unwrap();
    let status = Command::new("mkfifo")
        .arg(root.join("fifo"))
        .status()
        .unwrap();
    assert!(status.success());
    // Names that are not text.
    let odd: std::ffi::OsString =
        std::os::unix::ffi::OsStringExt::from_vec(b"odd-\xff\xfe-name\nwith-newline".to_vec());
    fs::write(root.join(odd), b"x").unwrap();
    fs::write(root.join("n".repeat(255)), b"long").unwrap();
    // A subvolume of its own, with files in it.
    fs::create_dir_all(root.join("nested-subvol/dir")).unwrap();
    fs::write(
        root.join("nested-subvol/dir/inside.bin"),
        pseudo_random(100_000, 9),
    )
    .unwrap();
    fs::write(root.join("nested-subvol/top.txt"), "nested ".repeat(5000)).unwrap();
}

fn make_image(scratch: &Scratch, compress: Option<&str>) -> (PathBuf, PathBuf) {
    let src = scratch.0.join("src");
    build_tree(&src);
    let img = scratch.0.join("fs.img");
    let f = fs::File::create(&img).unwrap();
    f.set_len(160 << 20).unwrap();
    drop(f);
    let mut cmd = Command::new("mkfs.btrfs");
    cmd.args(["-q", "-f", "--rootdir"])
        .arg(&src)
        .args(["--subvol", "nested-subvol"]);
    if let Some(c) = compress {
        cmd.args(["--compress", c]);
    }
    cmd.arg(&img);
    run(&mut cmd);
    (src, img)
}

/// What the directory the image was made from says about each path.
struct Oracle {
    kind: BTreeMap<String, NodeKind>,
    len: BTreeMap<String, u64>,
    mtime: BTreeMap<String, i64>,
    /// Paths grouped by the source inode, for the files with several names.
    links: Vec<Vec<String>>,
}

fn oracle(root: &Path) -> Oracle {
    let mut o = Oracle {
        kind: BTreeMap::new(),
        len: BTreeMap::new(),
        mtime: BTreeMap::new(),
        links: Vec::new(),
    };
    let mut by_ino: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let rel = if prefix.is_empty() {
                name.to_string_lossy().into_owned()
            } else {
                format!("{prefix}/{}", name.to_string_lossy())
            };
            let md = fs::symlink_metadata(entry.path()).unwrap();
            let ft = md.file_type();
            let kind = if ft.is_dir() {
                NodeKind::Dir
            } else if ft.is_symlink() {
                NodeKind::Symlink
            } else if ft.is_file() {
                NodeKind::File
            } else {
                assert!(ft.is_fifo());
                NodeKind::Other
            };
            o.kind.insert(rel.clone(), kind);
            o.mtime.insert(rel.clone(), md.mtime());
            o.len.insert(
                rel.clone(),
                if kind == NodeKind::Dir { 0 } else { md.len() },
            );
            if kind == NodeKind::File && md.nlink() > 1 {
                by_ino.entry(md.ino()).or_default().push(rel.clone());
            }
            if kind == NodeKind::Dir {
                stack.push((entry.path(), rel));
            }
        }
    }
    o.links = by_ino.into_values().collect();
    o
}

/// The sum of `nbytes` of every inode of every tree, which is what the
/// kernel's `st_blocks` is made from.
fn nbytes_of_all_inodes(src: &ImageSource) -> u64 {
    let mut total = 0;
    for tree in [5u64, 256] {
        src.search(tree, Key::MIN, Key::MAX, &mut |item| {
            if item.key.ty == 1 && item.key.objectid >= 256 && item.data.len() >= 160 {
                total += u64::from_le_bytes(item.data[24..32].try_into().unwrap());
            }
            bs_scan_btrfs::Flow::Continue
        })
        .unwrap();
    }
    total
}

fn check(compress: Option<&str>, name: &str) {
    let scratch = Scratch::new(name);
    let (srcdir, img) = make_image(&scratch, compress);
    let source = ImageSource::open(&img);
    let run = try_read(&source, START, &BtrfsOptions::default()).expect("the image is read");
    assert_eq!(run.applied_rejected, 0);
    assert_eq!(run.parked, 0);
    assert!(run.snapshot.scan_complete(), "{:?}", run.report.diagnostics);
    assert_eq!(
        run.report.diagnostics.total(),
        0,
        "{:?}",
        run.report.diagnostics
    );
    assert_eq!(run.report.trees, 2, "the nested subvolume was entered");
    let m = run.flat();
    let o = oracle(&srcdir);

    // Every path of the source is in the tree with its type, size and time,
    // and nothing else is.
    let mut missing = Vec::new();
    for (path, kind) in &o.kind {
        let Some(info) = m.get(path) else {
            missing.push(path.clone());
            continue;
        };
        let want_kind = if path == "nested-subvol" {
            NodeKind::Dir
        } else {
            *kind
        };
        assert_eq!(info.kind, want_kind, "{path}");
        if *kind != NodeKind::Dir {
            // Later names of a hard-linked file carry no bytes, by design.
            let duplicate = info.flags.contains(NodeFlags::HARD_LINK_DUPLICATE);
            let want = if duplicate { 0 } else { o.len[path] };
            assert_eq!(info.logical, want, "{path}: logical size");
            assert_eq!(info.mtime, o.mtime[path], "{path}: mtime");
        }
        if *kind == NodeKind::Dir {
            assert_eq!(info.logical, 0, "{path}");
            assert_eq!(info.allocated, 0, "{path}");
        }
    }
    assert!(missing.is_empty(), "missing from the tree: {missing:?}");
    let extra: Vec<_> = m.keys().filter(|k| !o.kind.contains_key(*k)).collect();
    assert!(extra.is_empty(), "not in the source: {extra:?}");

    // Hard links: every name flagged, one carries the bytes.
    assert!(!o.links.is_empty());
    for group in &o.links {
        for p in group {
            assert!(m[p].flags.contains(NodeFlags::HARD_LINK), "{p}");
        }
        let dupes = group
            .iter()
            .filter(|p| m[*p].flags.contains(NodeFlags::HARD_LINK_DUPLICATE))
            .count();
        assert_eq!(dupes, group.len() - 1, "{group:?}");
    }
    assert!(!m["link"].flags.contains(NodeFlags::HARD_LINK));

    // Nothing is shared on a freshly built image, so every extent is counted
    // at the one file that has it. Without compression the tree adds up to
    // the sum of the inodes' `nbytes`, which the kernel's `st_blocks` is made
    // from (`mkfs` records the uncompressed size there when it compresses, so
    // the comparison is only made for the plain image).
    assert_eq!(run.report.sharing.files, 0);
    let root = run.snapshot.root().unwrap();
    if compress.is_none() {
        assert_eq!(root.allocated(), nbytes_of_all_inodes(&source));
    }
    assert!(m.values().all(|i| !i.flags.contains(NodeFlags::SHARED)));

    // Inline data costs the bytes stored. (`mkfs` writes a sparse file out in
    // full, so holes are covered by the in-memory tests.)
    let small: u64 = ["sub/deeper/small", "hard-a", "sub/hard-b"]
        .iter()
        .map(|p| m[*p].allocated)
        .sum();
    assert_eq!(
        small, 3,
        "inline data, as stored, counted at one of the names"
    );
    assert_eq!(m["empty-file"].allocated, 0);
    // The target as stored: some writers add the terminating NUL.
    let stored = m["link"].allocated;
    assert!((16..=17).contains(&stored), "{stored}");
    assert!(m["random.bin"].allocated >= 300_000);
    assert_eq!(m["nested-subvol/dir/inside.bin"].logical, 100_000);

    if compress.is_some() {
        let packed = &m["sub/compressible.txt"];
        assert_eq!(packed.logical, 19 * 40_000);
        assert!(packed.allocated < packed.logical / 4, "{packed:?}");
        assert!(packed.flags.contains(NodeFlags::SPARSE_OR_COMPRESSED));
        assert!(run.report.compression.files >= 1);
        assert!(run.report.compression.on_disk < run.report.compression.logical);
    } else {
        assert_eq!(m["sub/compressible.txt"].allocated, 761_856);
        assert_eq!(run.report.compression.files, 0);
    }
}

#[test]
fn an_image_without_compression_reads_back_as_it_was_made() {
    check(None, "plain");
}

#[test]
fn an_image_with_zstd_compression_reports_logical_and_on_disk_sizes() {
    check(Some("zstd"), "zstd");
}

#[test]
fn reading_an_image_twice_gives_the_same_tree() {
    let scratch = Scratch::new("twice");
    let (_, img) = make_image(&scratch, Some("zlib"));
    let source = ImageSource::open(&img);
    let a = try_read(&source, START, &BtrfsOptions::default())
        .unwrap()
        .flat();
    let b = try_read(&source, START, &BtrfsOptions::default())
        .unwrap()
        .flat();
    let strip = |m: &BTreeMap<String, common::Info>| {
        m.iter()
            .map(|(k, v)| (k.clone(), v.logical, v.allocated, v.mtime, v.flags))
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(&a), strip(&b));
}
