//! NTFS images for the tests that run on Linux: made with `mkntfs`, filled
//! through an `ntfs-3g` mount (no privileges needed, only `/dev/fuse`), and
//! read back both by the raw reader and, while still mounted, by the walker.
//! `ntfsinfo` (also from ntfs-3g) is the independent oracle.
//!
//! Nothing here is committed as an image: every test builds its own, below
//! the build's temporary directory. Only the mount point, an empty directory,
//! lives in `/tmp`, because the FUSE helper refuses mounts elsewhere on hosts
//! that confine it.
#![allow(dead_code, unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

const EPOCH_BASE: u64 = 1_700_000_000;

#[derive(Clone, Debug)]
pub enum Item {
    Dir(String),
    /// A file of `len` bytes of a repeating pattern that does not compress.
    File(String, u64),
    /// A file of `len` bytes that holds data only in `(offset, len)` islands.
    Sparse(String, u64, Vec<(u64, u64)>),
    /// A file of `len` bytes of compressible text, written after the
    /// compression attribute was set on the empty file.
    Compressed(String, u64),
    /// A second name for the file at the first path.
    HardLink(String, String),
    /// ntfs-3g stores these as ordinary files that Windows does not treat as
    /// links.
    Symlink(String, String),
    /// An alternate data stream: file, stream name, length.
    Stream(String, String, u64),
}

pub fn dir(p: &str) -> Item {
    Item::Dir(p.into())
}

pub fn file(p: &str, len: u64) -> Item {
    Item::File(p.into(), len)
}

pub fn sparse(p: &str, len: u64, islands: &[(u64, u64)]) -> Item {
    Item::Sparse(p.into(), len, islands.to_vec())
}

pub fn compressed(p: &str, len: u64) -> Item {
    Item::Compressed(p.into(), len)
}

pub fn link(p: &str, to: &str) -> Item {
    Item::HardLink(p.into(), to.into())
}

pub fn symlink_to(p: &str, target: &str) -> Item {
    Item::Symlink(p.into(), target.into())
}

pub fn stream(p: &str, name: &str, len: u64) -> Item {
    Item::Stream(p.into(), name.into(), len)
}

#[derive(Clone, Debug)]
pub struct ImageSpec {
    pub items: Vec<Item>,
    /// `mkntfs -c` (bytes per cluster).
    pub cluster: Option<u32>,
    /// `mkntfs -s` (bytes per sector).
    pub sector: Option<u32>,
    pub size_mb: u64,
}

impl ImageSpec {
    pub fn new(items: Vec<Item>) -> ImageSpec {
        ImageSpec {
            items,
            cluster: None,
            sector: None,
            size_mb: 48,
        }
    }

    pub fn cluster(mut self, bytes: u32) -> Self {
        self.cluster = Some(bytes);
        self
    }

    pub fn sector(mut self, bytes: u32) -> Self {
        self.sector = Some(bytes);
        self
    }

    pub fn size_mb(mut self, mb: u64) -> Self {
        self.size_mb = mb;
        self
    }
}

/// A built image. While `mnt` is set the volume is mounted and the walker can
/// scan it; `unmount` ends that. Everything is removed on drop unless
/// `KEEP_FIXTURES` is set.
pub struct Fixture {
    pub root: PathBuf,
    pub img: PathBuf,
    pub mnt: Option<PathBuf>,
    mnt_path: PathBuf,
}

impl Fixture {
    /// Mounts the image again, read-only and with nothing cached, so a walker
    /// sees what is on the disk and not what the writing mount remembers.
    pub fn remount_read_only(&mut self) {
        self.unmount();
        let mnt = self.mnt_path.clone();
        fs::create_dir_all(&mnt).expect("mount point");
        // Recorded before the mount is tried, so that a mount that half
        // succeeds is still unmounted when the fixture is dropped.
        self.mnt = Some(mnt.clone());
        mount(&self.img, &mnt, true);
    }

    pub fn unmount(&mut self) {
        if let Some(mnt) = self.mnt.take() {
            unmount(&mnt);
        }
    }

    /// The mount point; panics when already unmounted.
    pub fn mount_point(&self) -> &Path {
        self.mnt.as_deref().expect("the image is mounted")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Nothing here may panic: this runs while a failed test unwinds, and
        // a second panic would abort the process and hide the first.
        if let Some(mnt) = self.mnt.take()
            && !try_unmount(&mnt)
        {
            eprintln!("could not unmount {}", mnt.display());
        }
        let _ = fs::remove_dir(&self.mnt_path);
        if std::env::var_os("KEEP_FIXTURES").is_none() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// Runs a tool that must exist; says so loudly when it does not.
pub fn run(cmd: &mut Command) -> String {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd.output().unwrap_or_else(|e| {
        panic!("the image tests need `{name}` (package ntfs-3g): {e}");
    });
    if !out.status.success() {
        panic!(
            "{name} failed: {}\n{}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn os(s: &str) -> &OsStr {
    OsStr::new(s)
}

/// Bytes that do not compress and do not repeat within a few kilobytes.
fn noise(len: u64, seed: u64) -> Vec<u8> {
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn text(len: u64) -> Vec<u8> {
    let line = b"the quick brown fox jumps over the lazy dog\n";
    (0..len)
        .map(|i| line[(i % line.len() as u64) as usize])
        .collect()
}

pub fn set_xattr(path: &Path, name: &str, value: &[u8]) {
    let p = CString::new(path.as_os_str().as_bytes()).expect("path");
    let n = CString::new(name).expect("name");
    // SAFETY: both strings are NUL-terminated and outlive the call; the value
    // pointer and length describe a live slice.
    let rc = unsafe {
        libc::setxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    assert_eq!(
        rc,
        0,
        "setxattr {name} on {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

fn write_item(mnt: &Path, item: &Item, serial: u64) {
    match item {
        Item::Dir(p) => fs::create_dir_all(mnt.join(os(p))).expect("create dir"),
        Item::File(p, len) => {
            let mut f = File::create(mnt.join(os(p))).expect("create file");
            f.write_all(&noise(*len, serial)).expect("write");
        }
        Item::Sparse(p, len, islands) => {
            let f = File::create(mnt.join(os(p))).expect("create sparse");
            f.set_len(*len).expect("set_len");
            for (offset, n) in islands {
                f.write_all_at(&noise(*n, serial + offset), *offset)
                    .expect("write island");
            }
        }
        Item::Compressed(p, len) => {
            // ntfs-3g compresses what is written into a directory marked
            // compressed (and refuses to mark a file that has data). The
            // flags are FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_COMPRESSED,
            // big-endian as the interface wants them.
            let at = mnt.join(os(p));
            let parent = at.parent().expect("a parent");
            fs::create_dir_all(parent).expect("create dir");
            set_xattr(parent, "system.ntfs_attrib_be", &[0, 0, 8, 0x10]);
            File::create(&at)
                .expect("create compressed")
                .write_all(&text(*len))
                .expect("write");
        }
        Item::HardLink(p, to) => fs::hard_link(mnt.join(os(to)), mnt.join(os(p))).expect("link"),
        Item::Symlink(p, target) => symlink(os(target), mnt.join(os(p))).expect("symlink"),
        Item::Stream(p, name, len) => {
            set_xattr(
                &mnt.join(os(p)),
                &format!("user.{name}"),
                &noise(*len, serial),
            );
        }
    }
}

/// Gives every entry a fixed, distinct modification time, deepest first so
/// that no later change moves a directory's time.
fn stamp(mnt: &Path) {
    let mut found: Vec<(PathBuf, bool)> = Vec::new();
    let mut stack = vec![mnt.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read fixture dir") {
            let entry = entry.expect("entry");
            let kind = entry.file_type().expect("file type");
            if kind.is_dir() {
                stack.push(entry.path());
            }
            found.push((entry.path(), kind.is_symlink()));
        }
    }
    found.sort_by(|a, b| {
        b.0.components()
            .count()
            .cmp(&a.0.components().count())
            .then_with(|| a.0.cmp(&b.0))
    });
    for (i, (path, is_link)) in found.iter().enumerate() {
        let secs = EPOCH_BASE + u64::try_from(i).unwrap_or(0) % 100_000 * 3 + 1;
        let when = UNIX_EPOCH + Duration::from_secs(secs);
        if *is_link {
            run(Command::new("touch")
                .args(["-h", "-d"])
                .arg(format!("@{secs}"))
                .arg(path));
        } else {
            File::open(path)
                .expect("open to stamp")
                .set_modified(when)
                .expect("set mtime");
        }
    }
    File::open(mnt)
        .expect("open root")
        .set_modified(UNIX_EPOCH + Duration::from_secs(EPOCH_BASE))
        .expect("set root mtime");
}

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "ntfs-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        name
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("scratch dir");
    root
}

fn mounted(mnt: &Path) -> bool {
    let want = mnt.to_string_lossy().into_owned();
    fs::read_to_string("/proc/self/mountinfo")
        .map(|t| {
            t.lines()
                .any(|l| l.split(' ').nth(4) == Some(want.as_str()))
        })
        .unwrap_or(false)
}

fn mount(img: &Path, mnt: &Path, read_only: bool) {
    let options = if read_only {
        "ro,compression,streams_interface=xattr,no_def_opts"
    } else {
        "compression,streams_interface=xattr,no_def_opts"
    };
    run(Command::new("ntfs-3g")
        .args(["-o", options])
        .arg(img)
        .arg(mnt)
        .env("LC_ALL", "C.UTF-8"));
    let begun = Instant::now();
    while !mounted(mnt) {
        assert!(
            begun.elapsed() < Duration::from_secs(20),
            "ntfs-3g did not mount {}",
            img.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn unmount(mnt: &Path) {
    assert!(try_unmount(mnt), "could not unmount {}", mnt.display());
}

/// Unmounts and removes the mount point; says whether it is gone. Never
/// panics.
fn try_unmount(mnt: &Path) -> bool {
    for tool in ["fusermount3", "fusermount"] {
        if Command::new(tool)
            .arg("-u")
            .arg(mnt)
            .output()
            .is_ok_and(|o| o.status.success())
        {
            break;
        }
    }
    let begun = Instant::now();
    while mounted(mnt) && begun.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let gone = !mounted(mnt);
    if gone {
        let _ = fs::remove_dir(mnt);
    }
    gone
}

/// Builds the image, mounts it, writes the items, stamps times, and leaves it
/// mounted.
pub fn build(name: &str, spec: &ImageSpec) -> Fixture {
    static M: AtomicU32 = AtomicU32::new(0);
    let root = scratch(name);
    let img = root.join("img");
    let mnt = PathBuf::from(format!(
        "/tmp/bsntfs-{}-{}",
        std::process::id(),
        M.fetch_add(1, Ordering::Relaxed)
    ));
    // The guard comes before anything that can fail, so whatever was made
    // before a failure (the scratch directory, the mount point, a mount) is
    // taken down by it.
    let mut fixture = Fixture {
        root,
        img: img.clone(),
        mnt: None,
        mnt_path: mnt.clone(),
    };
    let f = File::create(&img).expect("image file");
    f.set_len(spec.size_mb << 20).expect("size image");
    drop(f);
    let mut mkntfs = Command::new("mkntfs");
    mkntfs.args(["-F", "-Q", "-q", "-L", "fixture"]);
    if let Some(c) = spec.cluster {
        mkntfs.arg("-c").arg(c.to_string());
    }
    if let Some(s) = spec.sector {
        mkntfs.arg("-s").arg(s.to_string());
    }
    run(mkntfs.arg(&img));
    fs::create_dir_all(&mnt).expect("mount point");
    fixture.mnt = Some(mnt.clone());
    mount(&img, &mnt, false);
    // Directories first, so every file has its place; then the rest in the
    // order given (links after the files they name).
    for (i, item) in spec.items.iter().enumerate() {
        write_item(&mnt, item, i as u64 + 1);
    }
    stamp(&mnt);
    run(&mut Command::new("sync"));
    fixture
}

/// What `ntfsinfo` says about one data stream of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamFacts {
    pub name: String,
    pub resident: bool,
    pub size: u64,
    pub allocated: u64,
    pub compressed: Option<u64>,
    pub unit: u64,
}

/// All the `$DATA` streams of `path` (a `/`-rooted path in the image), from
/// `ntfsinfo`. The image must be unmounted.
pub fn oracle(img: &Path, path: &str) -> Vec<StreamFacts> {
    oracle_with_inode(img, path).1
}

/// The same, with the file's record number.
pub fn oracle_with_inode(img: &Path, path: &str) -> (u64, Vec<StreamFacts>) {
    let text = run(Command::new("ntfsinfo").args(["-F", path]).arg(img));
    let mut out: Vec<StreamFacts> = Vec::new();
    let mut current: Option<StreamFacts> = None;
    let mut inode = 0u64;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Dumping Inode ") {
            inode = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        }
        if line.starts_with("Dumping attribute") {
            if let Some(s) = current.take() {
                out.push(s);
            }
            if line.contains("$DATA (0x80)") {
                current = Some(StreamFacts {
                    name: String::new(),
                    resident: false,
                    size: 0,
                    allocated: 0,
                    compressed: None,
                    unit: 0,
                });
            }
            continue;
        }
        let Some(s) = current.as_mut() else { continue };
        let t = line.trim();
        let value = |t: &str| -> u64 {
            t.split(':')
                .nth(1)
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        };
        if let Some(rest) = t.strip_prefix("Resident:") {
            s.resident = rest.trim() == "Yes";
        } else if t.starts_with("Attribute name:") {
            s.name = t.split('\'').nth(1).unwrap_or("").to_string();
        } else if t.starts_with("Data size:") || t.starts_with("Data Size:") {
            s.size = value(t);
        } else if t.starts_with("Allocated size:") || t.starts_with("Allocated Size:") {
            s.allocated = value(t);
        } else if t.starts_with("Compressed size:") {
            s.compressed = Some(value(t));
        } else if t.starts_with("Compression unit:") {
            s.unit = value(t);
        }
    }
    if let Some(s) = current.take() {
        out.push(s);
    }
    (inode, out)
}

/// Bytes of clusters a stream occupies: none when it lives in the record;
/// the compressed size when the stream records one (compressed or sparse);
/// else its allocated size.
pub fn occupied(s: &StreamFacts) -> u64 {
    if s.resident {
        0
    } else {
        s.compressed.unwrap_or(s.allocated)
    }
}

/// The sizes a Windows directory listing reports for the unnamed stream:
/// its length, and its allocation, which for data kept in the file record is
/// the length rounded up to eight bytes, and otherwise the clusters it
/// occupies.
pub fn listing_sizes(streams: &[StreamFacts]) -> Option<(u64, u64)> {
    let s = streams.iter().find(|s| s.name.is_empty())?;
    let allocated = if s.resident {
        (s.size + 7) & !7
    } else {
        occupied(s)
    };
    Some((s.size, allocated))
}

pub fn flat_oracle(img: &Path, paths: &[String]) -> BTreeMap<String, Vec<StreamFacts>> {
    paths
        .iter()
        .map(|p| (p.clone(), oracle(img, &format!("/{p}"))))
        .collect()
}

/// Clusters held, in bytes, by the non-resident attributes of `path` that are
/// not data (a directory's index buffers and bitmap, an attribute list).
/// From `ntfsinfo`; the image must be unmounted.
pub fn attribute_allocation(img: &Path, path: &str) -> u64 {
    allocation_of(img, path, None)
}

/// Clusters held by a directory's `$INDEX_ALLOCATION`, in bytes.
pub fn index_allocation(img: &Path, path: &str) -> u64 {
    allocation_of(img, path, Some("$INDEX_ALLOCATION"))
}

fn allocation_of(img: &Path, path: &str, only: Option<&str>) -> u64 {
    let text = run(Command::new("ntfsinfo").args(["-F", path]).arg(img));
    let mut total = 0u64;
    let mut counting = false;
    let mut non_resident = false;
    for line in text.lines() {
        if line.starts_with("Dumping attribute") {
            counting = !line.contains("$DATA (0x80)") && only.is_none_or(|o| line.contains(o));
            non_resident = false;
            continue;
        }
        let t = line.trim();
        if t.starts_with("Resident:") {
            non_resident = t.contains("No");
        }
        if counting
            && non_resident
            && (t.starts_with("Allocated size:") || t.starts_with("Allocated Size:"))
        {
            total += t
                .split(':')
                .nth(1)
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    total
}

/// How many `$FILE_NAME` attributes (hard links, DOS names included) and
/// whether an `$ATTRIBUTE_LIST` the file has, from `ntfsinfo`.
pub fn name_and_list_counts(img: &Path, path: &str) -> (usize, bool) {
    let text = run(Command::new("ntfsinfo").args(["-F", path]).arg(img));
    let names = text.matches("$FILE_NAME (0x30)").count();
    (names, text.contains("$ATTRIBUTE_LIST (0x20)"))
}
