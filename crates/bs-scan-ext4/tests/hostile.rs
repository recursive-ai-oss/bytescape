//! The reader against hostile disks.
//!
//! Real images are built once, then thousands of corrupted variants are read:
//! bit flips and length, offset, count and pointer rewrites in the superblock,
//! group descriptors, inodes, directory blocks, extent nodes and indirect
//! blocks, cross-linked and self-referencing directories, truncated devices.
//! The corruption is seeded, so a failing case is reproduced by its seed.
//!
//! For every variant the reader must: not panic; finish within a time bound;
//! stay within a memory bound (measured by a counting allocator); and either
//! return a typed error or produce a tree that the aggregator accepts and
//! that passes its own consistency check, with every record applied.
//!
//! `HOSTILE_SEEDS` raises the number of seeds per family (default 150);
//! `HOSTILE_ONLY=base:family:seed` runs one case verbosely.
#![allow(unsafe_code)]
// The reader and the tools these tests drive are Linux-only.
#![cfg(target_os = "linux")]

mod common;

use bs_scan::{ScanContext, ScanEnd, ScanError, ScanOptions, Scanner, start};
use bs_scan_ext4::{ByteSource, Ext4Error, Ext4Options, Ext4Report, read_filesystem};
use common::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---- a counting allocator, switched on around a read ----
//
// It counts every thread's allocations while a read runs: the reader's own
// threads and the tree's applier included, because all of that is memory the
// scan takes.

struct Counting;

static ON: AtomicBool = AtomicBool::new(false);
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn track_add(n: usize) {
    if ON.load(Ordering::Relaxed) {
        let now = CURRENT.fetch_add(n, Ordering::Relaxed).saturating_add(n);
        PEAK.fetch_max(now, Ordering::Relaxed);
    }
}

fn track_sub(n: usize) {
    if ON.load(Ordering::Relaxed) {
        // Memory allocated before the counting began can be freed during it.
        let _ = CURRENT.try_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
            Some(c.saturating_sub(n))
        });
    }
}

// SAFETY: every method forwards to the system allocator with the layout it
// was given, so the system allocator's contract holds unchanged; the counters
// are atomics and allocate nothing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        track_add(layout.size());
        // SAFETY: same layout as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        track_add(layout.size());
        // SAFETY: same layout as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        track_sub(layout.size());
        // SAFETY: `ptr` and `layout` are the caller's, from this allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        track_sub(layout.size());
        track_add(new_size);
        // SAFETY: `ptr` and `layout` are the caller's, from this allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

// ---- corrupted images as patches over a clean one ----

#[derive(Clone, Debug)]
struct Patched {
    base: Arc<Vec<u8>>,
    patches: Vec<(u64, Vec<u8>)>,
    len: u64,
    /// Byte ranges that fail to read, as a dying disk's would.
    unreadable: Vec<(u64, u64)>,
}

impl ByteSource for Patched {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|&e| e <= self.len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "past the end"))?;
        if self
            .unreadable
            .iter()
            .any(|&(from, to)| offset < to && end > from)
        {
            return Err(io::Error::from_raw_os_error(5));
        }
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let src = self
            .base
            .get(start..start + buf.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "past the base"))?;
        buf.copy_from_slice(src);
        for (at, bytes) in &self.patches {
            let patch_end = at + bytes.len() as u64;
            if *at < end && patch_end > offset {
                let from = offset.max(*at);
                let to = end.min(patch_end);
                let dst = (from - offset) as usize..(to - offset) as usize;
                let srcp = (from - at) as usize..(to - at) as usize;
                buf[dst].copy_from_slice(&bytes[srcp]);
            }
        }
        Ok(())
    }
}

// ---- seeded randomness ----

#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }

    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        items.get(usize::try_from(self.below(items.len() as u64)).ok()?)
    }
}

// ---- where the interesting structures of a base image lie ----

struct BaseImage {
    name: &'static str,
    image: Arc<Vec<u8>>,
    block_size: u64,
    blocks: u64,
    inode_size: u64,
    inodes_in_use: u64,
    inodes_per_group: u64,
    first_data_block: u64,
    /// Start of each group's inode table, in blocks.
    tables: Vec<u64>,
    dir_blocks: Vec<u64>,
    node_blocks: Vec<u64>,
    dir_inodes: Vec<u64>,
    gdt_start: u64,
    desc_size: u64,
    clean_nodes: u32,
}

fn le32(b: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes(b[at..at + 4].try_into().unwrap()))
}

fn le16(b: &[u8], at: usize) -> u64 {
    u64::from(u16::from_le_bytes(b[at..at + 2].try_into().unwrap()))
}

fn analyse(name: &'static str, spec: &ImageSpec, dirs: &[&str], files: &[&str]) -> BaseImage {
    let fixture = build(name, spec);
    let image = Arc::new(std::fs::read(&fixture.img).unwrap());
    let sb = &image[1024..2048];
    let log = le32(sb, 0x18);
    let block_size = 1024u64 << log;
    let blocks = le32(sb, 0x04);
    let inodes_per_group = le32(sb, 0x28);
    let inode_size = le16(sb, 0x58);
    let first_data_block = le32(sb, 0x14);
    let incompat = le32(sb, 0x60);
    let desc_size = if incompat & 0x80 != 0 {
        le16(sb, 0xFE)
    } else {
        32
    };
    let groups = (blocks - first_data_block).div_ceil(le32(sb, 0x20));
    let gdt_start = (first_data_block + 1) * block_size;
    let mut tables = Vec::new();
    for g in 0..groups {
        let at = (gdt_start + g * desc_size) as usize;
        let d = &image[at..at + desc_size as usize];
        let lo = le32(d, 8);
        let hi = if desc_size >= 64 { le32(d, 0x28) } else { 0 };
        let t = lo | (hi << 32);
        // With meta_bg the descriptors after the first group of them are not
        // here; what does not name a block is left out.
        tables.push(if t < blocks { t } else { first_data_block + 1 });
    }
    // debugfs knows where the directories' blocks and the map nodes are.
    let mut script = String::new();
    for d in dirs {
        script.push_str(&format!("blocks {d}\nstat {d}\n"));
    }
    for f in files {
        script.push_str(&format!("stat {f}\n"));
    }
    let script_path = fixture.root.join("layout.cmd");
    std::fs::write(&script_path, script).unwrap();
    let text = run(std::process::Command::new("debugfs")
        .arg("-f")
        .arg(&script_path)
        .arg(&fixture.img));
    let mut dir_blocks = Vec::new();
    let mut node_blocks = Vec::new();
    let mut dir_inodes = Vec::new();
    let mut after_blocks = false;
    for line in text.lines() {
        if let Some(cmd) = line.strip_prefix("debugfs: ") {
            after_blocks = cmd.starts_with("blocks");
            continue;
        }
        if after_blocks {
            dir_blocks.extend(
                line.split_whitespace()
                    .filter_map(|w| w.parse::<u64>().ok()),
            );
            after_blocks = false;
        }
        if let Some(rest) = line.strip_prefix("Inode: ")
            && line.contains("Type: directory")
            && let Some(n) = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
        {
            dir_inodes.push(n);
        }
        // Extent tree nodes and indirect blocks show up in the block list
        // of `stat` as (ETB0):N, (IND):N, (DIND):N and (TIND):N.
        for item in line.split(", ") {
            let item = item.trim();
            for tag in ["(ETB", "(IND):", "(DIND):", "(TIND):"] {
                if let Some(rest) = item.strip_prefix(tag)
                    && let Some((_, n)) = rest.rsplit_once(':').or(Some(("", rest)))
                    && let Ok(n) = n.trim().parse::<u64>()
                {
                    node_blocks.push(n);
                }
            }
        }
    }
    dir_inodes.sort_unstable();
    dir_inodes.dedup();
    eprintln!(
        "{name}: {} directory blocks, {} map nodes, {} directories",
        dir_blocks.len(),
        node_blocks.len(),
        dir_inodes.len()
    );
    let clean = scan_image(&fixture.img);
    assert_sound(&clean.result);
    let report = clean.report.unwrap();
    assert_eq!(
        report.diagnostics.total(),
        0,
        "{name}: the clean image must be clean"
    );
    BaseImage {
        name,
        image,
        block_size,
        blocks,
        inode_size,
        inodes_in_use: report.inodes_in_use,
        inodes_per_group,
        first_data_block,
        tables,
        dir_blocks,
        node_blocks,
        dir_inodes,
        gdt_start,
        desc_size,
        clean_nodes: clean.result.snapshot.len(),
    }
}

impl BaseImage {
    fn inode_offset(&self, ino: u64) -> Option<u64> {
        let index = ino.checked_sub(1)?;
        let (g, i) = (index / self.inodes_per_group, index % self.inodes_per_group);
        let table = *self.tables.get(usize::try_from(g).ok()?)?;
        Some(table * self.block_size + i * self.inode_size)
    }
}

// ---- mutations ----

fn interesting(rng: &mut Rng, l: &BaseImage) -> u64 {
    let specials = [
        0,
        1,
        2,
        3,
        4,
        11,
        12,
        255,
        256,
        4095,
        4096,
        65535,
        65536,
        0x7FFF_FFFF,
        0x8000_0000,
        0xFFFF_FFFF,
        l.blocks,
        l.blocks - 1,
        l.blocks + 1,
        l.blocks / 2,
        l.inodes_in_use + 11,
        u64::MAX,
    ];
    match rng.below(5) {
        0 | 1 => *rng.pick(&specials).unwrap(),
        2 => rng.below(l.blocks.max(1)),
        3 => rng.below(1 << 16),
        _ => rng.next(),
    }
}

fn poke(patches: &mut Vec<Patch>, at: u64, width: u64, value: u64) {
    let bytes = value.to_le_bytes();
    let w = width.clamp(1, 8) as usize;
    patches.push((at, bytes[..w].to_vec()));
}

/// Rewrites a few numbers inside one structure chosen by the family.
/// Bytes to write at an offset.
type Patch = (u64, Vec<u8>);

fn corrupt(l: &BaseImage, family: &str, rng: &mut Rng) -> (Vec<Patch>, u64, Vec<(u64, u64)>) {
    let mut patches = Vec::new();
    let mut len = l.image.len() as u64;
    let mut unreadable = Vec::new();
    let times = 1 + rng.below(6);
    for _ in 0..times {
        match family {
            "superblock" => {
                let at = if rng.below(4) == 0 {
                    l.gdt_start + rng.below(l.desc_size * l.tables.len().min(4) as u64)
                } else {
                    1024 + rng.below(0x280)
                };
                let w = *rng.pick(&[1u64, 2, 4, 4, 8]).unwrap();
                let v = interesting(rng, l);
                poke(&mut patches, at, w, v);
            }
            "inodes" => {
                let ino = match rng.below(6) {
                    0 => 2,
                    1..=3 => *rng.pick(&l.dir_inodes).unwrap_or(&2),
                    _ => 1 + rng.below(l.inodes_in_use + 12),
                };
                if let Some(base) = l.inode_offset(ino) {
                    let fields: [u64; 14] = [
                        0x00, 0x04, 0x10, 0x1A, 0x1C, 0x20, 0x28, 0x2C, 0x30, 0x34, 0x6C, 0x74,
                        0x80, 0x88,
                    ];
                    let off = if rng.below(4) == 0 {
                        rng.below(l.inode_size)
                    } else {
                        *rng.pick(&fields).unwrap()
                    };
                    let w = *rng.pick(&[1u64, 2, 4, 4]).unwrap();
                    let v = interesting(rng, l);
                    poke(&mut patches, base + off, w, v);
                }
            }
            "dirents" => {
                if let Some(&block) = rng.pick(&l.dir_blocks) {
                    let start = block * l.block_size;
                    // Walk the block's own chain of entries, to land on fields.
                    let mut offsets = vec![0u64];
                    let mut at = 0u64;
                    while at + 8 <= l.block_size && offsets.len() < 400 {
                        let p = (start + at) as usize;
                        let rec = le16(&l.image, p + 4);
                        if rec < 8 || at + rec > l.block_size {
                            break;
                        }
                        at += rec;
                        offsets.push(at);
                    }
                    let entry = *rng.pick(&offsets).unwrap();
                    let field = *rng.pick(&[0u64, 4, 6, 7, 8]).unwrap();
                    let (w, v) = match field {
                        0 => (4, interesting(rng, l)),
                        4 => (2, rng.below(70_000)),
                        6 | 7 => (1, rng.below(256)),
                        _ => (1, rng.below(256)),
                    };
                    poke(&mut patches, start + entry + field, w, v);
                }
            }
            "nodes" => {
                let pool = if l.node_blocks.is_empty() {
                    &l.dir_blocks
                } else {
                    &l.node_blocks
                };
                if let Some(&block) = rng.pick(pool) {
                    let field = *rng
                        .pick(&[0u64, 2, 4, 6, 12, 16, 20, 24, 28, 32, 36, 40])
                        .unwrap();
                    let w = *rng.pick(&[2u64, 4, 4]).unwrap();
                    let v = interesting(rng, l);
                    poke(&mut patches, block * l.block_size + field, w, v);
                }
            }
            "crosslinks" => {
                // Make one directory's block map a copy of another's, or
                // point it at itself.
                let a = *rng.pick(&l.dir_inodes).unwrap_or(&2);
                let b = if rng.below(2) == 0 {
                    *rng.pick(&l.dir_inodes).unwrap_or(&2)
                } else {
                    1 + rng.below(l.inodes_in_use + 12)
                };
                if let (Some(oa), Some(ob)) = (l.inode_offset(a), l.inode_offset(b)) {
                    let src = (ob + 0x28) as usize;
                    patches.push((oa + 0x28, l.image[src..src + 60].to_vec()));
                    if rng.below(3) == 0 {
                        // Same blocks, and a size that wants far more.
                        poke(&mut patches, oa + 4, 4, rng.below(1 << 30));
                    }
                }
            }
            "flips" => {
                let region = rng.below(4);
                let at = match region {
                    0 => 1024 + rng.below(1024),
                    1 => l.gdt_start + rng.below(l.desc_size * l.tables.len() as u64),
                    2 => {
                        let ino = 1 + rng.below(l.inodes_in_use + 12);
                        l.inode_offset(ino).unwrap_or(0) + rng.below(l.inode_size)
                    }
                    _ => {
                        let pool = if l.dir_blocks.is_empty() {
                            &l.node_blocks
                        } else {
                            &l.dir_blocks
                        };
                        let b = rng.pick(pool).copied().unwrap_or(l.first_data_block + 1);
                        b * l.block_size + rng.below(l.block_size)
                    }
                };
                if (at as usize) < l.image.len() {
                    let byte = l.image[at as usize] ^ (1u8 << rng.below(8));
                    patches.push((at, vec![byte]));
                }
            }
            "unreadable" => {
                let block = match rng.below(4) {
                    0 => l.first_data_block + 1,
                    1 => *rng.pick(&l.tables).unwrap_or(&1),
                    2 => *rng.pick(&l.dir_blocks).unwrap_or(&1),
                    _ => *rng.pick(&l.node_blocks).unwrap_or(&1),
                };
                let blocks = 1 + rng.below(4);
                let from = block * l.block_size;
                unreadable.push((from, from + blocks * l.block_size));
            }
            "truncate" => {
                len = if rng.below(3) == 0 {
                    rng.below(4096)
                } else {
                    rng.below(l.image.len() as u64)
                };
            }
            other => panic!("unknown family {other}"),
        }
    }
    (patches, len, unreadable)
}

// ---- running one case ----

struct Probe {
    peak: usize,
    panicked: Option<String>,
    outcome: Option<Result<(ScanEnd, Ext4Report), Ext4Error>>,
    elapsed: Duration,
}

struct Harness {
    source: Patched,
    probe: Mutex<Probe>,
    threads: usize,
}

impl Scanner for Harness {
    fn name(&self) -> &'static str {
        "ext4-hostile"
    }

    fn scan(&self, _root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        CURRENT.store(0, Ordering::Relaxed);
        PEAK.store(0, Ordering::Relaxed);
        ON.store(true, Ordering::Relaxed);
        let begun = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            read_filesystem(
                &self.source,
                b"/",
                &Ext4Options {
                    threads: Some(self.threads),
                    ..Ext4Options::default()
                },
                ctx,
            )
        }));
        let elapsed = begun.elapsed();
        ON.store(false, Ordering::Relaxed);
        let peak = PEAK.load(Ordering::Relaxed);
        let mut probe = self.probe.lock().unwrap();
        probe.peak = peak;
        probe.elapsed = elapsed;
        match outcome {
            Ok(r) => {
                let end = match &r {
                    Ok((end, _)) => Ok(*end),
                    Err(e) => Err(ScanError::Root(io::Error::new(
                        io::ErrorKind::InvalidData,
                        e.to_string(),
                    ))),
                };
                probe.outcome = Some(r);
                end
            }
            Err(payload) => {
                let text = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "panic".into());
                probe.panicked = Some(text);
                Err(ScanError::Unsupported)
            }
        }
    }
}

const TIME_LIMIT: Duration = Duration::from_secs(60);

/// Reads one corrupted image and checks everything that must hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum Class {
    /// A typed error.
    Refused = 0,
    /// A tree, with problems reported or entries marked unreadable.
    Damaged = 1,
    /// A tree and no complaints (the change did not matter).
    Untouched = 2,
}

fn check(
    label: &str,
    source: Patched,
    memory_bound: usize,
    threads: usize,
) -> (Class, usize, Duration) {
    let harness = Arc::new(Harness {
        threads,
        source,
        probe: Mutex::new(Probe {
            peak: 0,
            panicked: None,
            outcome: None,
            elapsed: Duration::ZERO,
        }),
    });
    let handle = start(
        harness.clone() as Arc<dyn Scanner>,
        PathBuf::from("/"),
        ScanOptions::default(),
    );
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.wait());
    });
    let result = rx
        .recv_timeout(TIME_LIMIT)
        .unwrap_or_else(|_| panic!("{label}: the read did not finish within {TIME_LIMIT:?}"));
    let probe = harness.probe.lock().unwrap();
    if let Some(p) = &probe.panicked {
        panic!("{label}: the reader panicked: {p}");
    }
    assert!(
        probe.peak <= memory_bound,
        "{label}: peak memory {} B exceeds the bound {memory_bound} B",
        probe.peak
    );
    assert!(
        probe.elapsed < Duration::from_secs(30),
        "{label}: took {:?}",
        probe.elapsed
    );
    let (peak, elapsed) = (probe.peak, probe.elapsed);
    let class = match probe.outcome.as_ref() {
        Some(Err(_)) => Class::Refused, // A typed error is an allowed answer.
        Some(Ok((end, report))) => {
            assert_eq!(result.stats.records_parked, 0, "{label}: parked records");
            assert_eq!(
                result.stats.records_rejected, 0,
                "{label}: rejected records"
            );
            result
                .snapshot
                .verify()
                .unwrap_or_else(|e| panic!("{label}: tree invariants: {e:?}"));
            if *end == ScanEnd::Completed {
                assert!(
                    result.snapshot.scan_complete(),
                    "{label}: tree not complete"
                );
            }
            if report.diagnostics.total() > 0 || report.unreadable_entries > 0 {
                Class::Damaged
            } else {
                Class::Untouched
            }
        }
        None => panic!("{label}: no outcome"),
    };
    (class, peak, elapsed)
}

fn bases() -> Vec<BaseImage> {
    let mut wide = vec![dir("wide"), dir("sub"), dir("sub/deeper")];
    // Long names make the directory many blocks long, and files between its
    // growth steps leave it in pieces, so it needs an extent tree node or
    // indirect blocks.
    for i in 0..500 {
        let name = format!("wide/entry_{i:04}_{}", "x".repeat(90));
        wide.push(file(&name, (i % 5 + 1) as u64 * 2500));
    }
    wide.extend([
        file("sub/a.txt", 100),
        file("sub/deeper/b.mkv", 20_000),
        link("sub/hard", "sub/a.txt"),
        symlink_to("sub/sym", "a.txt"),
        sparse(
            "sub/frag",
            3_000_000,
            &(0..30).map(|i| (i * 90_000, 4096)).collect::<Vec<_>>(),
        ),
        file("big", 600_000),
    ]);
    let ext4 = ImageSpec::new(wide.clone()).size("16M").indexed();
    let blockmap = ImageSpec::new(wide.clone())
        .args(&[
            "-O",
            "^extents,^flex_bg,^64bit,^metadata_csum,^orphan_file,^resize_inode,^huge_file",
        ])
        .size("16M");
    let small = ImageSpec::new(wide.clone())
        .args(&["-b", "1024", "-I", "128", "-O", "^metadata_csum,^64bit"])
        .size("16M")
        .indexed();
    let mut inline_items = vec![dir("a"), dir("b"), file("a/x", 5), file("a/y", 6), dir("c")];
    for n in ["c1", "c2", "c3", "c4"] {
        inline_items.push(file(&format!("c/{n}"), 3));
    }
    inline_items.extend(wide.iter().take(60).cloned());
    let inline = ImageSpec::new(inline_items)
        .args(&["-O", "inline_data"])
        .size("16M");
    let dirs = ["/", "/wide", "/sub", "/sub/deeper"];
    let files = ["/sub/frag", "/big"];
    // Layouts the bases above do not have: 64 KiB blocks, descriptors spread
    // over the file system (meta_bg), and clusters of several blocks
    // (bigalloc). They carry fewer names to keep the images small.
    let few: Vec<Item> = wide
        .iter()
        .filter(|i| !i.path().starts_with(b"wide/"))
        .cloned()
        .chain(
            wide.iter()
                .filter(|i| i.path().starts_with(b"wide/"))
                .take(120)
                .cloned(),
        )
        .collect();
    let k64 = ImageSpec::new(few.clone())
        .args(&["-b", "65536"])
        .size("96M")
        .indexed();
    let metabg = ImageSpec::new(few.clone())
        .args(&[
            "-b",
            "1024",
            "-g",
            "256",
            "-O",
            "meta_bg,^resize_inode,sparse_super2,^metadata_csum",
        ])
        .size("24M");
    let bigalloc = ImageSpec::new(few.clone())
        .args(&["-O", "bigalloc,^resize_inode", "-C", "16384"])
        .size("48M");
    vec![
        analyse("ext4", &ext4, &dirs, &files),
        analyse("blockmap", &blockmap, &dirs, &files),
        analyse("small", &small, &dirs, &files),
        analyse("inline", &inline, &["/", "/a", "/c", "/wide"], &["/big"]),
        analyse("k64", &k64, &dirs, &files),
        analyse("metabg", &metabg, &dirs, &files),
        analyse("bigalloc", &bigalloc, &dirs, &files),
    ]
}

fn seeds() -> u64 {
    std::env::var("HOSTILE_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(150)
}

const FAMILIES: [&str; 8] = [
    "superblock",
    "inodes",
    "dirents",
    "nodes",
    "crosslinks",
    "flips",
    "unreadable",
    "truncate",
];

/// Memory a read of a corrupted image may take. The tables hold at most a
/// 12-byte entry and a name byte for every 12 bytes of directory the image
/// can hold, and 32 bytes for every inode its tables have room for, so about
/// twice the image's size covers them; the rest is fixed overhead (thread
/// stacks, batches in flight, the tree built from them).
fn memory_bound(image_len: u64) -> usize {
    usize::try_from(image_len)
        .unwrap_or(usize::MAX)
        .saturating_mul(2)
        .saturating_add(16 << 20)
}

#[test]
fn every_family_of_corruption_is_survived() {
    let only = std::env::var("HOSTILE_ONLY").ok();
    let mut cases = 0u64;
    let mut errors = 0u64;
    let mut worst_peak = 0usize;
    let mut worst_time = Duration::ZERO;
    let mut tally: std::collections::BTreeMap<(&str, &str), [u64; 3]> = Default::default();
    for l in bases() {
        for family in FAMILIES {
            let seed_base: u64 = std::env::var("HOSTILE_SEED_BASE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            for seed in seed_base..seed_base + seeds() {
                let id = format!("{}:{family}:{seed}", l.name);
                if only.as_ref().is_some_and(|o| *o != id) {
                    continue;
                }
                let mut rng = Rng::new(seed.wrapping_add(1_000_003 * (family.len() as u64)));
                let (patches, len, unreadable) = corrupt(&l, family, &mut rng);
                if only.is_some() {
                    eprintln!("{id}: {} patches, length {len}", patches.len());
                    eprintln!("  unreadable ranges: {unreadable:?}");
                    for (at, b) in &patches {
                        eprintln!("  @{at:#x}: {b:02x?}");
                    }
                }
                let source = Patched {
                    base: l.image.clone(),
                    patches,
                    len,
                    unreadable,
                };
                let (class, peak, elapsed) =
                    check(&id, source, memory_bound(len), 1 + (seed % 4) as usize);
                worst_peak = worst_peak.max(peak);
                worst_time = worst_time.max(elapsed);
                tally.entry((l.name, family)).or_default()[class as usize] += 1;
                cases += 1;
            }
        }
        // The clean image still reads, so the harness is not vacuous.
        let clean = Patched {
            base: l.image.clone(),
            patches: Vec::new(),
            len: l.image.len() as u64,
            unreadable: Vec::new(),
        };
        let scan = start(
            Arc::new(Harness {
                threads: 4,
                source: clean,
                probe: Mutex::new(Probe {
                    peak: 0,
                    panicked: None,
                    outcome: None,
                    elapsed: Duration::ZERO,
                }),
            }),
            PathBuf::from("/"),
            ScanOptions::default(),
        )
        .wait();
        assert_eq!(scan.snapshot.len(), l.clean_nodes, "{}: clean tree", l.name);
        errors += u64::from(scan.stats.error.is_some());
    }
    eprintln!("{cases} corrupted images read without a panic");
    eprintln!("worst peak memory {worst_peak} B, slowest read {worst_time:?}");
    eprintln!("{:<10} {:<11} refused damaged untouched", "image", "family");
    for ((image, family), [refused, damaged, untouched]) in &tally {
        eprintln!("{image:<10} {family:<11} {refused:>7} {damaged:>7} {untouched:>9}");
    }
    // A mutator that never makes the reader notice anything proves nothing.
    if only.is_none() {
        for family in FAMILIES {
            let noticed: u64 = tally
                .iter()
                .filter(|((_, f), _)| *f == family)
                .map(|(_, c)| c[0] + c[1])
                .sum();
            assert!(
                noticed > 0,
                "family {family} never made the reader notice damage"
            );
        }
    }
    assert_eq!(errors, 0);
    assert!(only.is_some() || cases >= 4 * 8 * 150 || seeds() != 150);
}
