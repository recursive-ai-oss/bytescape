//! Fixture images for the differential and corruption tests: a declarative
//! tree is written to a directory, turned into an ext4 image with `mke2fs -d`
//! (no privileges needed), and read back both by the raw reader and by the
//! directory walker. `debugfs` is the independent oracle.
//!
//! Nothing here is committed as an image: every test builds its own, below the
//! build's temporary directory (never `/tmp`).
#![allow(dead_code)]

use bs_scan::{ScanOptions, ScanResult, Walker, start};
use bs_scan_ext4::{Ext4Options, Ext4Report, Ext4Scanner};
use bs_tree::{NodeFlags, NodeId, NodeKind, NodeRef, Snapshot};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, UNIX_EPOCH};

/// The first modification time given to fixture entries.
const EPOCH_BASE: u64 = 1_700_000_000;

/// One thing in a fixture tree. Paths are relative to the tree's root and may
/// hold any bytes except NUL and `/` inside a component.
#[derive(Clone, Debug)]
pub enum Item {
    Dir(Vec<u8>),
    /// A file of `len` bytes of a repeating pattern.
    File(Vec<u8>, u64),
    /// A file of `len` bytes that holds data only in `(offset, len)` islands.
    Sparse(Vec<u8>, u64, Vec<(u64, u64)>),
    Symlink(Vec<u8>, Vec<u8>),
    /// A second name for the file at the first path.
    HardLink(Vec<u8>, Vec<u8>),
    Fifo(Vec<u8>),
}

impl Item {
    pub fn path(&self) -> &[u8] {
        match self {
            Item::Dir(p)
            | Item::File(p, _)
            | Item::Sparse(p, _, _)
            | Item::Symlink(p, _)
            | Item::HardLink(p, _)
            | Item::Fifo(p) => p,
        }
    }
}

pub fn dir(p: &str) -> Item {
    Item::Dir(p.as_bytes().to_vec())
}

pub fn file(p: &str, len: u64) -> Item {
    Item::File(p.as_bytes().to_vec(), len)
}

pub fn sparse(p: &str, len: u64, islands: &[(u64, u64)]) -> Item {
    Item::Sparse(p.as_bytes().to_vec(), len, islands.to_vec())
}

pub fn link(p: &str, to: &str) -> Item {
    Item::HardLink(p.as_bytes().to_vec(), to.as_bytes().to_vec())
}

pub fn symlink_to(p: &str, target: &str) -> Item {
    Item::Symlink(p.as_bytes().to_vec(), target.as_bytes().to_vec())
}

/// How the image is made.
#[derive(Clone, Debug)]
pub struct ImageSpec {
    /// Extra `mke2fs` arguments, e.g. `["-O", "inline_data"]`.
    pub mke2fs: Vec<String>,
    /// Image size, as `mke2fs` takes it.
    pub size: String,
    /// Run `e2fsck -fyD` afterwards, which indexes large directories.
    pub index_dirs: bool,
    pub items: Vec<Item>,
}

impl ImageSpec {
    pub fn new(items: Vec<Item>) -> ImageSpec {
        ImageSpec {
            mke2fs: Vec::new(),
            size: "32M".into(),
            index_dirs: false,
            items,
        }
    }

    pub fn args(mut self, args: &[&str]) -> ImageSpec {
        self.mke2fs.extend(args.iter().map(|a| (*a).to_owned()));
        self
    }

    pub fn size(mut self, size: &str) -> ImageSpec {
        self.size = size.into();
        self
    }

    pub fn indexed(mut self) -> ImageSpec {
        self.index_dirs = true;
        self
    }
}

/// A built image and the directory it was made from. Removed on drop unless
/// `KEEP_FIXTURES` is set.
pub struct Fixture {
    pub root: PathBuf,
    pub src: PathBuf,
    pub img: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::env::var_os("KEEP_FIXTURES").is_none() {
            let _ = Command::new("rm").arg("-rf").arg(&self.root).status();
        }
    }
}

/// Runs a tool that must exist; says so loudly when it does not.
pub fn run(cmd: &mut Command) -> String {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd.output().unwrap_or_else(|e| {
        panic!("the image tests need `{name}` (package e2fsprogs): {e}");
    });
    if !out.status.success() {
        panic!(
            "{name} failed: {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn os(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
}

fn pattern(len: u64) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 7 + i / 251) % 251).unwrap_or(0) | 1)
        .collect()
}

fn write_item(src: &Path, item: &Item) {
    let at = src.join(os(item.path()));
    match item {
        Item::Dir(_) => fs::create_dir_all(&at).expect("create dir"),
        Item::File(_, len) => {
            // One write, which makes it less likely that a `sync` run by a test
            // building its own image at the same time lands halfway and makes
            // the host file system allocate the file in pieces. It cannot rule
            // it out: see `explain_host_extent_index`.
            let mut f = File::create(&at).expect("create file");
            let mut data = Vec::new();
            let mut left = *len;
            while left > 0 {
                let n = left.min(1 << 20);
                data.extend_from_slice(&pattern(n));
                left -= n;
            }
            f.write_all(&data).expect("write");
        }
        Item::Sparse(_, len, islands) => {
            let f = File::create(&at).expect("create sparse");
            f.set_len(*len).expect("set_len");
            for (offset, n) in islands {
                f.write_all_at(&pattern(*n), *offset).expect("write island");
            }
        }
        Item::Symlink(_, target) => symlink(os(target), &at).expect("symlink"),
        Item::HardLink(_, to) => fs::hard_link(src.join(os(to)), &at).expect("hard link"),
        Item::Fifo(_) => {
            run(Command::new("mkfifo").arg(&at));
        }
    }
}

/// Gives every entry a fixed, distinct modification time, deepest first so
/// that no later change moves a directory's time.
fn stamp(src: &Path) {
    use std::os::unix::fs::FileTypeExt;
    let mut found: Vec<(PathBuf, fs::FileType)> = Vec::new();
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read fixture dir") {
            let entry = entry.expect("entry");
            let kind = entry.file_type().expect("file type");
            if kind.is_dir() {
                stack.push(entry.path());
            }
            found.push((entry.path(), kind));
        }
    }
    found.sort_by(|a, b| {
        b.0.components()
            .count()
            .cmp(&a.0.components().count())
            .then_with(|| a.0.cmp(&b.0))
    });
    for (i, (path, kind)) in found.iter().enumerate() {
        let secs = EPOCH_BASE + u64::try_from(i).unwrap_or(0) % 100_000 * 3 + 1;
        if kind.is_symlink() || kind.is_fifo() {
            run(Command::new("touch")
                .args(["-h", "-d"])
                .arg(format!("@{secs}"))
                .arg(path));
        } else {
            File::open(path)
                .expect("open to stamp")
                .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
                .expect("set mtime");
        }
    }
    File::open(src)
        .expect("open root")
        .set_modified(UNIX_EPOCH + Duration::from_secs(EPOCH_BASE))
        .expect("set root mtime");
}

/// A fresh scratch directory for one test, below the build's temporary one.
fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "ext4-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        name
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("scratch dir");
    root
}

pub fn build(name: &str, spec: &ImageSpec) -> Fixture {
    let root = scratch(name);
    let src = root.join("src");
    let img = root.join("img");
    fs::create_dir_all(&src).expect("src dir");
    for item in &spec.items {
        write_item(&src, item);
    }
    stamp(&src);
    // Let delayed allocation settle so block counts of the source are final.
    let _ = Command::new("sync").status();
    let mut mke2fs = Command::new("mke2fs");
    if std::env::var_os("BYTESCAPE_FIXTURE_HOST_MKE2FS").is_none() {
        mke2fs.env(
            "MKE2FS_CONFIG",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/common/mke2fs.conf"),
        );
    }
    mke2fs.args(["-q", "-t", "ext4", "-F", "-d"]).arg(&src);
    mke2fs.args(&spec.mke2fs).arg(&img).arg(&spec.size);
    run(&mut mke2fs);
    if spec.index_dirs {
        // Exit code 1 means "modified"; that is the point.
        let out = Command::new("e2fsck")
            .args(["-fyD"])
            .arg(&img)
            .output()
            .expect("e2fsck");
        assert!(
            out.status.code().is_some_and(|c| c <= 1),
            "e2fsck -D failed: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    Fixture { root, src, img }
}

/// How many in-use inodes nothing names on a fresh image: the orphan file,
/// on file systems that have one, and nothing otherwise. Read from the
/// superblock, so the answer follows the image, not the host's mke2fs.
pub fn orphan_file_inodes(image: &[u8]) -> u64 {
    const COMPAT_ORPHAN_FILE: u32 = 0x1000;
    let compat = image
        .get(1024 + 0x5C..1024 + 0x60)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map_or(0, u32::from_le_bytes);
    u64::from(compat & COMPAT_ORPHAN_FILE != 0)
}

/// What a node is, as far as the comparison goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub id: NodeId,
    pub kind: NodeKind,
    /// For a directory: its own entry only, without what lies below it.
    pub logical: u64,
    pub allocated: u64,
    pub mtime: i64,
    pub flags: NodeFlags,
    pub category: u8,
    pub failed: bool,
    pub origin: Option<NodeId>,
}

/// Every node below the root, by path relative to it.
pub fn flatten(snap: &Snapshot) -> BTreeMap<Vec<u8>, Info> {
    let mut out = BTreeMap::new();
    let Some(root) = snap.root() else {
        return out;
    };
    let mut stack: Vec<(NodeRef<'_>, Vec<u8>)> = vec![(root, Vec::new())];
    while let Some((node, path)) = stack.pop() {
        for child in node.children() {
            let mut p = path.clone();
            if !p.is_empty() {
                p.push(b'/');
            }
            p.extend_from_slice(child.name());
            out.insert(
                p.clone(),
                Info {
                    id: child.id(),
                    kind: child.kind(),
                    logical: child.logical(),
                    allocated: child.allocated(),
                    mtime: child.mtime(),
                    flags: child.flags(),
                    category: child.category().0,
                    failed: child.is_unreadable(),
                    origin: child.hard_link_origin(),
                },
            );
            if child.kind() == NodeKind::Dir {
                // The tree sums a directory with everything below it; the
                // entry's own size is what is left when the children go.
                let (mut below_logical, mut below_allocated) = (0u64, 0u64);
                for grandchild in child.children() {
                    below_logical += grandchild.logical();
                    below_allocated += grandchild.allocated();
                }
                if let Some(info) = out.get_mut(&p) {
                    info.logical = child.logical() - below_logical;
                    info.allocated = child.allocated() - below_allocated;
                }
                stack.push((child, p));
            }
        }
    }
    out
}

pub fn scan_walker(src: &Path) -> ScanResult {
    Walker::scan_blocking(src, ScanOptions::default())
}

pub struct ImageScan {
    pub result: ScanResult,
    pub report: Option<Ext4Report>,
}

pub fn scan_image_with(img: &Path, options: Ext4Options, scan: ScanOptions) -> ImageScan {
    let scanner = Arc::new(Ext4Scanner::new(img, options));
    let handle = start(scanner.clone(), PathBuf::from("/"), scan);
    let result = handle.wait();
    ImageScan {
        result,
        report: scanner.last_report(),
    }
}

pub fn scan_image(img: &Path) -> ImageScan {
    scan_image_with(img, Ext4Options::default(), ScanOptions::default())
}

/// The facts `debugfs` gives about one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Oracle {
    pub is_dir: bool,
    pub is_regular: bool,
    pub size: u64,
    pub links: u64,
    pub blocks512: u64,
    pub mtime: i64,
}

/// Asks `debugfs` about every path (only paths made of plain characters can
/// be put on its command line safely; others are left out of the answer).
pub fn oracle(img: &Path, paths: &[Vec<u8>]) -> BTreeMap<Vec<u8>, Oracle> {
    let plain = |p: &&Vec<u8>| {
        p.iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+' | b'/'))
    };
    let wanted: Vec<&Vec<u8>> = paths.iter().filter(plain).collect();
    let script = wanted
        .iter()
        .map(|p| format!("stat /{}\n", String::from_utf8_lossy(p)))
        .collect::<String>();
    let script_path = img.with_extension("cmd");
    fs::write(&script_path, script).expect("write debugfs script");
    let text = run(Command::new("debugfs").arg("-f").arg(&script_path).arg(img));
    let mut out = BTreeMap::new();
    let mut current: Option<Vec<u8>> = None;
    let mut facts = Oracle {
        is_dir: false,
        is_regular: false,
        size: 0,
        links: 0,
        blocks512: 0,
        mtime: 0,
    };
    let flush = |current: &mut Option<Vec<u8>>, facts: &Oracle, out: &mut BTreeMap<_, _>| {
        if let Some(path) = current.take() {
            out.insert(path, facts.clone());
        }
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("debugfs: stat /") {
            flush(&mut current, &facts, &mut out);
            current = Some(rest.as_bytes().to_vec());
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if line.starts_with("Inode:") {
            facts.is_dir = line.contains("Type: directory");
            facts.is_regular = line.contains("Type: regular");
        }
        if let Some(i) = words.iter().position(|w| *w == "Size:")
            && line.starts_with("User:")
        {
            facts.size = words.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(0);
        }
        if let Some(i) = words.iter().position(|w| *w == "Links:") {
            facts.links = words.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(0);
            if let Some(j) = words.iter().position(|w| *w == "Blockcount:") {
                facts.blocks512 = words.get(j + 1).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
        }
        if let Some(rest) = line.trim_start().strip_prefix("mtime: 0x") {
            let hex = rest.split_whitespace().next().unwrap_or("0:0");
            let (lo, extra) = hex.split_once(':').unwrap_or((hex, "0"));
            let lo = u32::from_str_radix(lo, 16).unwrap_or(0);
            let extra = u32::from_str_radix(extra, 16).unwrap_or(0);
            facts.mtime = i64::from(lo as i32) + (i64::from(extra & 3) << 32);
        }
    }
    flush(&mut current, &facts, &mut out);
    out
}

/// A difference between two trees at one path.
#[derive(Clone, Debug)]
pub struct Diff {
    pub path: String,
    pub field: &'static str,
    pub reader: String,
    pub other: String,
}

/// Why a difference is allowed. Every exception names the field it covers
/// and gives its reason; a difference without one fails the test.
pub struct Exception {
    pub field: &'static str,
    pub reason: &'static str,
    /// Whether the exception covers this path.
    pub covers: fn(&str, &Info) -> bool,
}

pub fn lossy(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

/// Compares the reader's tree with another tree (the walker's) path by path.
/// Hard-linked names are compared as groups, because which name carries the
/// bytes depends on the order names are met in.
pub fn compare_trees(reader: &Snapshot, other: &Snapshot, skip_prefixes: &[&[u8]]) -> Vec<Diff> {
    let a = flatten(reader);
    let b = flatten(other);
    let skipped = |p: &[u8]| {
        skip_prefixes
            .iter()
            .any(|s| p == *s || (p.starts_with(s) && p.get(s.len()) == Some(&b'/')))
    };
    let mut diffs = Vec::new();
    let mut push = |p: &[u8], field: &'static str, r: String, o: String| {
        diffs.push(Diff {
            path: lossy(p),
            field,
            reader: r,
            other: o,
        });
    };
    for (path, x) in &a {
        if skipped(path) {
            continue;
        }
        let Some(y) = b.get(path) else {
            push(path, "presence", "present".into(), "missing".into());
            continue;
        };
        if x.kind != y.kind {
            push(
                path,
                "kind",
                format!("{:?}", x.kind),
                format!("{:?}", y.kind),
            );
            continue;
        }
        let relevant = NodeFlags::HARD_LINK
            | NodeFlags::SPARSE_OR_COMPRESSED
            | NodeFlags::NOT_FOLLOWED
            | NodeFlags::UNREADABLE
            | NodeFlags::SHARED;
        let fx = x.flags.bits() & relevant.bits();
        let fy = y.flags.bits() & relevant.bits();
        if fx != fy {
            push(path, "flags", format!("{fx:#x}"), format!("{fy:#x}"));
        }
        if x.mtime != y.mtime {
            push(path, "mtime", x.mtime.to_string(), y.mtime.to_string());
        }
        if x.category != y.category {
            push(
                path,
                "category",
                x.category.to_string(),
                y.category.to_string(),
            );
        }
        let linked = x.flags.contains(NodeFlags::HARD_LINK);
        if !linked {
            if x.logical != y.logical {
                push(
                    path,
                    "logical",
                    x.logical.to_string(),
                    y.logical.to_string(),
                );
            }
            if x.allocated != y.allocated {
                push(
                    path,
                    "allocated",
                    x.allocated.to_string(),
                    y.allocated.to_string(),
                );
            }
        }
    }
    for path in b.keys() {
        if !skipped(path) && !a.contains_key(path) {
            push(path, "presence", "missing".into(), "present".into());
        }
    }
    // Hard-link groups: the same names, the same total bytes.
    let groups = |m: &BTreeMap<Vec<u8>, Info>| {
        let by_id: BTreeMap<NodeId, &Vec<u8>> = m.iter().map(|(p, i)| (i.id, p)).collect();
        let mut groups: BTreeMap<Vec<Vec<u8>>, (u64, u64)> = BTreeMap::new();
        let mut members: BTreeMap<NodeId, Vec<Vec<u8>>> = BTreeMap::new();
        let mut bytes: BTreeMap<NodeId, (u64, u64)> = BTreeMap::new();
        for (p, i) in m {
            if skipped(p) || !i.flags.contains(NodeFlags::HARD_LINK) {
                continue;
            }
            let head = i.origin.unwrap_or(i.id);
            members.entry(head).or_default().push(p.clone());
            let e = bytes.entry(head).or_default();
            e.0 += i.logical;
            e.1 += i.allocated;
        }
        let _ = by_id;
        for (head, mut names) in members {
            names.sort();
            groups.insert(names, bytes[&head]);
        }
        groups
    };
    let (ga, gb) = (groups(&a), groups(&b));
    for (names, bytes) in &ga {
        let label = lossy(names.first().map_or(&[][..], |n| n));
        match gb.get(names) {
            None => diffs.push(Diff {
                path: label,
                field: "link group",
                reader: format!("{} names", names.len()),
                other: "no such group".into(),
            }),
            Some(o) if o != bytes => diffs.push(Diff {
                path: label,
                field: "link group bytes",
                reader: format!("{bytes:?}"),
                other: format!("{o:?}"),
            }),
            _ => {}
        }
    }
    for names in gb.keys() {
        if !ga.contains_key(names) {
            diffs.push(Diff {
                path: lossy(names.first().map_or(&[][..], |n| n)),
                field: "link group",
                reader: "no such group".into(),
                other: format!("{} names", names.len()),
            });
        }
    }
    diffs
}

/// Splits differences into those an exception covers and those it does not.
pub fn apply_exceptions<'a>(
    diffs: Vec<Diff>,
    tree: &Snapshot,
    table: &[Exception],
) -> (Vec<(Diff, &'a str)>, Vec<Diff>) {
    let infos = flatten(tree);
    let mut allowed = Vec::new();
    let mut rest = Vec::new();
    'next: for d in diffs {
        let key = d.path.as_bytes().to_vec();
        if let Some(info) = infos.get(&key) {
            for e in table {
                if e.field == d.field && (e.covers)(&d.path, info) {
                    allowed.push((d, e.reason));
                    continue 'next;
                }
            }
        }
        rest.push(d);
    }
    (allowed, rest)
}

/// Allocated-size differences that the host's layout of the walker's source
/// copy explains, taken out of `diffs`; each one is printed with its reason.
///
/// The walker scans a copy of the tree on the host file system, and the
/// reader scans the image `mke2fs` made from it. On an ext4 host, a regular
/// file the host allocated in more than four pieces (the inode holds four
/// extents) carries one extra block, the extent tree's leaf, in its block
/// count. How a host splits a file depends on what else is writing to it at
/// the time and on how fragmented its free space is, so the same fixture is
/// sometimes one block larger to the walker than the contiguous file in the
/// image. (Measured on an ext4 host: files written the same way and then
/// synced showed 5864 sectors, or 5872 once the host had split them into five
/// or more extents.) The image is not wrong: `debugfs` checks every reader
/// size against the image exactly, in the same tests.
///
/// A difference is explained only when all of this holds, so that nothing
/// else is let through: the host file system is ext4; the path is a regular
/// file on the host; the reader's size is at least five blocks, since fewer
/// cannot be in five extents; and the walker's size is the reader's plus
/// exactly one host block. Any other size difference stays.
pub fn explain_host_extent_index(src: &Path, diffs: Vec<Diff>) -> Vec<Diff> {
    // Only ext4 counts an extent tree's blocks this way.
    let host_is_ext4 = stat_f(src, "%T").is_some_and(|t| t == "ext2/ext3");
    let host_block: u64 = if host_is_ext4 {
        stat_f(src, "%S").and_then(|t| t.parse().ok()).unwrap_or(0)
    } else {
        0
    };
    let mut rest = Vec::new();
    for d in diffs {
        let sizes = (d.reader.parse::<u64>().ok(), d.other.parse::<u64>().ok());
        if d.field == "allocated"
            && host_block != 0
            && let (Some(reader), Some(walker)) = sizes
            && walker == reader.saturating_add(host_block)
            && reader >= host_block.saturating_mul(5)
            && fs::symlink_metadata(src.join(os(d.path.as_bytes()))).is_ok_and(|m| m.is_file())
        {
            eprintln!(
                "explained: {:?} allocated: the walker counted {walker} B and the image holds \
                 {reader} B; the host copy is at least five blocks, so a host that split it into \
                 more than four extents counts the extent tree's leaf block ({host_block} B) too",
                d.path
            );
            continue;
        }
        rest.push(d);
    }
    rest
}

/// One field of `stat -f`, for the file system holding `path`.
fn stat_f(path: &Path, format: &str) -> Option<String> {
    let out = Command::new("stat")
        .args(["-f", "-c", format])
        .arg(path)
        .output()
        .ok()?;
    Some(String::from_utf8(out.stdout).ok()?.trim().to_owned())
}

/// Fails the test with every uncovered difference listed.
pub fn assert_no_diffs(what: &str, diffs: &[Diff]) {
    if diffs.is_empty() {
        return;
    }
    let mut text = format!("{what}: {} differences\n", diffs.len());
    for d in diffs.iter().take(40) {
        text.push_str(&format!(
            "  {:?} {}: reader {} / other {}\n",
            d.path, d.field, d.reader, d.other
        ));
    }
    panic!("{text}");
}

/// Checks the invariants of any finished scan.
pub fn assert_sound(result: &ScanResult) {
    assert!(
        result.stats.error.is_none(),
        "scan failed: {:?}",
        result.stats.error
    );
    assert_eq!(result.stats.records_parked, 0, "parents go before children");
    assert_eq!(result.stats.records_rejected, 0);
    result.snapshot.verify().expect("tree invariants");
}
