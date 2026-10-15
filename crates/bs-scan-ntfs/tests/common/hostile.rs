//! The harness for the corruption tests: a clean volume, seeded mutations of
//! it expressed as patches over the clean bytes, and a run that checks the
//! reader's promises for every variant: it does not panic, it ends within a
//! time bound, it stays within a memory bound (a counting allocator), and it
//! either refuses the volume with an error or produces a tree that passes the
//! tree's own consistency check.
//!
//! The corruption is seeded, so a failing case is reproduced by its seed.
#![allow(dead_code, unsafe_code, clippy::too_many_arguments)]

use crate::common::testkit::{SourceScanner, Spec, Volume};
use bs_scan::{ScanError, ScanOptions, start};
use bs_scan_ntfs::{ByteSource, NtfsOptions, NtfsReport};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

// ---- a counting allocator, switched on around a read ----
//
// It counts every thread's allocations while a read runs: the reader's own
// threads and the tree's applier included, because all of that is memory the
// scan takes.

pub struct Counting;

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

// ---- corrupted volumes as patches over a clean one ----

#[derive(Clone)]
pub struct Patched {
    pub base: Arc<dyn ByteSource>,
    pub patches: Vec<(u64, Vec<u8>)>,
    pub len: u64,
    /// Byte ranges that fail to read, as a dying disk's would.
    pub unreadable: Vec<(u64, u64)>,
}

impl Patched {
    pub fn clean(base: Arc<dyn ByteSource>) -> Patched {
        let len = base.len();
        Patched {
            base,
            patches: Vec::new(),
            len,
            unreadable: Vec::new(),
        }
    }
}

impl ByteSource for Patched {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        if end > self.len {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        for &(lo, hi) in &self.unreadable {
            if offset < hi && end > lo {
                return Err(io::Error::other("injected read error"));
            }
        }
        self.base.read_at(offset, buf)?;
        for (at, bytes) in &self.patches {
            let (p_lo, p_hi) = (*at, *at + bytes.len() as u64);
            let lo = offset.max(p_lo);
            let hi = end.min(p_hi);
            if lo < hi {
                let dst = (lo - offset) as usize;
                let src = (lo - p_lo) as usize;
                let n = (hi - lo) as usize;
                buf[dst..dst + n].copy_from_slice(&bytes[src..src + n]);
            }
        }
        Ok(())
    }
}

// ---- seeded randomness ----

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03);
        for _ in 0..4 {
            r.next();
        }
        r
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    pub fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

// ---- what is in the clean volume, as far as mutation needs to know ----

#[derive(Clone, Debug)]
pub struct AttrAt {
    /// Offset of the attribute header in the record (after fixups).
    pub at: usize,
    pub len: usize,
    pub ty: u32,
    pub non_resident: bool,
    pub name_len: usize,
    /// Offset of the value (resident) or the mapping pairs (non-resident).
    pub body: usize,
    pub body_len: usize,
    /// For a non-resident attribute: where its first run starts on the
    /// device, and the size the attribute states.
    pub first_run: Option<u64>,
    pub stated_size: u64,
}

#[derive(Clone, Debug)]
pub struct RecAt {
    /// Offset of the record on the device.
    pub off: u64,
    pub number: u64,
    pub in_use: bool,
    pub attrs: Vec<AttrAt>,
}

pub struct Base {
    pub name: String,
    /// In-use files the clean volume has that no directory reaches.
    pub baseline_unreachable: u64,
    pub src: Arc<dyn ByteSource>,
    pub records: Vec<RecAt>,
    pub record_bytes: usize,
    pub table_start: u64,
}

impl Base {
    /// The attribute lists that are kept in clusters: the record's offset,
    /// the attribute, and where its data starts on the device.
    pub fn stored_lists(&self) -> Vec<(&RecAt, &AttrAt, u64)> {
        let mut out = Vec::new();
        for r in &self.records {
            for a in &r.attrs {
                if a.ty == 0x20
                    && a.non_resident
                    && let Some(at) = a.first_run
                {
                    out.push((r, a, at));
                }
            }
        }
        out
    }
}

fn le16(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes([b[at], b[at + 1]]) as usize
}

fn le32(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
}

/// Where the first run of a list of mapping pairs starts on the device.
fn first_run_start(pairs: &[u8], cluster: u64) -> Option<u64> {
    let header = *pairs.first()?;
    let (len_size, off_size) = (usize::from(header & 0xF), usize::from(header >> 4));
    if len_size == 0 || off_size == 0 || off_size > 8 {
        return None;
    }
    let off = pairs.get(1 + len_size..1 + len_size + off_size)?;
    let mut b = [if off[off_size - 1] & 0x80 != 0 {
        0xFF
    } else {
        0
    }; 8];
    b[..off_size].copy_from_slice(off);
    u64::try_from(i64::from_le_bytes(b))
        .ok()?
        .checked_mul(cluster)
}

/// Finds the file records in the first part of the device and the
/// attributes in each, from the clean volume.
pub fn survey(name: &str, src: Arc<dyn ByteSource>) -> Base {
    let mut boot = vec![0u8; 512];
    src.read_at(0, &mut boot).expect("boot sector");
    let bps = le16(&boot, 0x0B) as u64;
    let spc = u64::from(boot[0x0D]);
    let cluster = bps * spc;
    let mft_lcn = u64::from_le_bytes(boot[0x30..0x38].try_into().unwrap());
    let table_start = mft_lcn * cluster;
    let record_bytes = 1usize << (256 - u32::from(boot[0x40]));
    let scan_len = src.len().min(table_start + (12 << 20));
    let mut records = Vec::new();
    let mut off = table_start;
    let mut buf = vec![0u8; record_bytes];
    while off + record_bytes as u64 <= scan_len {
        src.read_at(off, &mut buf).unwrap();
        if &buf[..4] == b"FILE" {
            let mut rec = buf.clone();
            let usa = le16(&rec, 4);
            let count = le16(&rec, 6);
            let sane = usa >= 0x2A && count == record_bytes / 512 + 1 && usa + count * 2 <= 512;
            if sane {
                for i in 1..count {
                    let tail = i * 512 - 2;
                    rec[tail] = rec[usa + 2 * i];
                    rec[tail + 1] = rec[usa + 2 * i + 1];
                }
                let flags = le16(&rec, 0x16);
                let mut attrs = Vec::new();
                let mut at = le16(&rec, 0x14);
                while at + 16 <= record_bytes {
                    let ty = le32(&rec, at);
                    if ty == 0xFFFF_FFFF {
                        break;
                    }
                    let len = le32(&rec, at + 4);
                    if len < 0x10 || at + len > record_bytes {
                        break;
                    }
                    let non_resident = rec[at + 8] != 0;
                    let name_len = rec[at + 9] as usize;
                    let (first_run, stated_size) = if non_resident && at + 0x38 <= record_bytes {
                        let size =
                            u64::from_le_bytes(rec[at + 0x30..at + 0x38].try_into().unwrap());
                        let runs = le16(&rec, at + 0x20);
                        let pairs = rec.get(at + runs..at + len).unwrap_or(&[]);
                        (first_run_start(pairs, cluster), size)
                    } else {
                        (None, 0)
                    };
                    let (body, body_len) = if non_resident {
                        let runs = le16(&rec, at + 0x20);
                        (at + runs, len.saturating_sub(runs))
                    } else {
                        (at + le16(&rec, at + 0x14), le32(&rec, at + 0x10))
                    };
                    attrs.push(AttrAt {
                        at,
                        len,
                        ty: ty as u32,
                        non_resident,
                        name_len,
                        body,
                        body_len,
                        first_run,
                        stated_size,
                    });
                    at += len;
                }
                records.push(RecAt {
                    off,
                    number: (off - table_start) / record_bytes as u64,
                    in_use: flags & 1 != 0,
                    attrs,
                });
            }
        }
        off += record_bytes as u64;
    }
    assert!(!records.is_empty(), "{name}: no records found");
    let baseline_unreachable = {
        let out = run_variant(Patched::clean(src.clone()), 1, u64::MAX);
        out.report.map_or(0, |r| r.unreachable.files)
    };
    Base {
        name: name.to_string(),
        baseline_unreachable,
        src,
        records,
        record_bytes,
        table_start,
    }
}

// ---- the mutators ----

pub type Patch = (u64, Vec<u8>);

/// A value for a field of `width` bytes that a hostile disk might hold.
fn value(rng: &mut Rng, width: usize, original: u64) -> u64 {
    let max = if width >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * width)) - 1
    };
    match rng.below(12) {
        0 => 0,
        1 => 1,
        2 => max,
        3 => max - 1,
        4 => max >> 1,
        5 => (max >> 1) + 1,
        6 => original.wrapping_add(1) & max,
        7 => original.wrapping_sub(1) & max,
        8 => rng.below(64),
        9 => rng.below(1 << 16) & max,
        10 => original ^ (1 << rng.below(8 * width as u64)),
        _ => rng.next() & max,
    }
}

fn field(rng: &mut Rng, base: &Base, at: u64, width: usize) -> Patch {
    let mut cur = vec![0u8; width];
    let _ = base.src.read_at(at, &mut cur);
    let mut orig = [0u8; 8];
    orig[..width.min(8)].copy_from_slice(&cur[..width.min(8)]);
    let v = value(rng, width, u64::from_le_bytes(orig));
    (at, v.to_le_bytes()[..width.min(8)].to_vec())
}

fn pick_record<'a>(rng: &mut Rng, base: &'a Base, must_have_attrs: bool) -> &'a RecAt {
    let used: Vec<&RecAt> = base
        .records
        .iter()
        .filter(|r| r.in_use && (!must_have_attrs || !r.attrs.is_empty()))
        .collect();
    let pool: &[&RecAt] = &used;
    if pool.is_empty() || rng.chance(10) {
        return rng.pick(&base.records);
    }
    pool[rng.below(pool.len() as u64) as usize]
}

fn pick_attr<'a>(rng: &mut Rng, rec: &'a RecAt, types: &[u32]) -> Option<&'a AttrAt> {
    let hits: Vec<&AttrAt> = rec
        .attrs
        .iter()
        .filter(|a| types.is_empty() || types.contains(&a.ty))
        .collect();
    if hits.is_empty() {
        None
    } else {
        Some(hits[rng.below(hits.len() as u64) as usize])
    }
}

pub const FAMILIES: &[&str] = &[
    "boot",
    "record-header",
    "fixup",
    "attr-header",
    "runlist",
    "file-name",
    "attribute-list",
    "mft-record",
    "bit-flips",
    "garbage",
    "device",
    // Appended, so the seeds of the families above stay what they were.
    "stored-list",
];

const BOOT_FIELDS: &[(u64, usize)] = &[
    (3, 8),
    (0x0B, 2),
    (0x0D, 1),
    (0x28, 8),
    (0x30, 8),
    (0x38, 8),
    (0x40, 1),
    (0x44, 1),
    (0x48, 8),
    (0x1FE, 2),
];
const REC_FIELDS: &[(u64, usize)] = &[
    (0, 4),
    (4, 2),
    (6, 2),
    (8, 8),
    (0x10, 2),
    (0x12, 2),
    (0x14, 2),
    (0x16, 2),
    (0x18, 4),
    (0x1C, 4),
    (0x20, 8),
    (0x28, 2),
];
const ATTR_COMMON: &[(usize, usize)] =
    &[(0, 4), (4, 4), (8, 1), (9, 1), (0xA, 2), (0xC, 2), (0xE, 2)];
const ATTR_RESIDENT: &[(usize, usize)] = &[(0x10, 4), (0x14, 2), (0x16, 1)];
const ATTR_NONRES: &[(usize, usize)] = &[
    (0x10, 8),
    (0x18, 8),
    (0x20, 2),
    (0x22, 1),
    (0x28, 8),
    (0x30, 8),
    (0x38, 8),
    (0x40, 8),
];
const NAME_VALUE: &[(usize, usize)] = &[
    (0, 8),
    (0x28, 8),
    (0x30, 8),
    (0x38, 4),
    (0x40, 1),
    (0x41, 1),
    (0x42, 2),
];
const LIST_ENTRY: &[(usize, usize)] = &[(0, 4), (4, 2), (6, 1), (7, 1), (8, 8), (16, 8), (24, 2)];

pub fn mutate(family: &str, rng: &mut Rng, base: &Base) -> Patched {
    let mut p = Patched::clean(base.src.clone());
    let n = 1 + rng.below(3);
    for _ in 0..n {
        match family {
            "boot" => {
                let &(at, w) = rng.pick(BOOT_FIELDS);
                p.patches.push(field(rng, base, at, w));
            }
            "record-header" => {
                let rec = pick_record(rng, base, false);
                let &(at, w) = rng.pick(REC_FIELDS);
                p.patches.push(field(rng, base, rec.off + at, w));
            }
            "fixup" => {
                let rec = pick_record(rng, base, false);
                let sectors = (base.record_bytes / 512) as u64;
                match rng.below(4) {
                    0 => p
                        .patches
                        .push((rec.off + 0x30, rng.next().to_le_bytes()[..2].to_vec())),
                    1 => {
                        let s = 1 + rng.below(sectors);
                        p.patches.push((
                            rec.off + s * 512 - 2,
                            rng.next().to_le_bytes()[..2].to_vec(),
                        ));
                    }
                    2 => {
                        let i = 1 + rng.below(sectors);
                        p.patches.push((
                            rec.off + 0x30 + 2 * i,
                            rng.next().to_le_bytes()[..2].to_vec(),
                        ));
                    }
                    _ => p.patches.push(field(rng, base, rec.off + 6, 2)),
                }
            }
            "attr-header" => {
                let rec = pick_record(rng, base, true);
                if let Some(a) = pick_attr(rng, rec, &[]) {
                    let table = match rng.below(3) {
                        0 => ATTR_COMMON,
                        1 if !a.non_resident => ATTR_RESIDENT,
                        _ if a.non_resident => ATTR_NONRES,
                        _ => ATTR_COMMON,
                    };
                    let &(at, w) = rng.pick(table);
                    p.patches
                        .push(field(rng, base, rec.off + (a.at + at) as u64, w));
                }
            }
            "runlist" => {
                let rec = pick_record(rng, base, true);
                let nonres: Vec<&AttrAt> = rec
                    .attrs
                    .iter()
                    .filter(|a| a.non_resident && a.body_len > 1)
                    .collect();
                if let Some(a) = nonres.get(rng.below(nonres.len().max(1) as u64) as usize) {
                    for _ in 0..1 + rng.below(4) {
                        let at = rec.off + (a.body + rng.below(a.body_len as u64) as usize) as u64;
                        let v = match rng.below(5) {
                            0 => 0u8,
                            1 => 0xFF,
                            2 => 0x88,
                            3 => 0x11,
                            _ => rng.next() as u8,
                        };
                        p.patches.push((at, vec![v]));
                    }
                } else {
                    // No run list here: damage a data attribute's size instead.
                    p.patches.push(field(rng, base, rec.off + 0x18, 4));
                }
            }
            "file-name" => {
                let rec = pick_record(rng, base, true);
                if let Some(a) = pick_attr(rng, rec, &[0x30]) {
                    let &(at, w) = rng.pick(NAME_VALUE);
                    if rng.chance(5) {
                        // A parent that is a different directory, or itself.
                        let target = match rng.below(4) {
                            0 => rec.number,
                            1 => 5,
                            2 => rng.below(base.records.len() as u64 + 8),
                            _ => rng.next() & 0x0000_FFFF_FFFF_FFFF,
                        };
                        let seq = rng.below(8) << 48;
                        p.patches.push((
                            rec.off + a.body as u64,
                            (target | seq).to_le_bytes().to_vec(),
                        ));
                    } else {
                        p.patches
                            .push(field(rng, base, rec.off + (a.body + at) as u64, w));
                    }
                }
            }
            "attribute-list" => {
                let rec = pick_record(rng, base, true);
                if let Some(a) = pick_attr(rng, rec, &[0x20]) {
                    if !a.non_resident && a.body_len > 0 {
                        let entries = (a.body_len / 0x20).max(1) as u64;
                        let e = rng.below(entries) as usize * 0x20;
                        let &(at, w) = rng.pick(LIST_ENTRY);
                        p.patches
                            .push(field(rng, base, rec.off + (a.body + e + at) as u64, w));
                    } else {
                        p.patches
                            .push(field(rng, base, rec.off + (a.at + 0x30) as u64, 8));
                    }
                } else {
                    // A file with no list: give it one that lies.
                    let rec = pick_record(rng, base, true);
                    p.patches.push(field(rng, base, rec.off + 0x16, 2));
                }
            }
            "stored-list" => {
                // An attribute list kept in clusters: its entries, its stated
                // size, its runs, and the clusters themselves.
                let lists = base.stored_lists();
                if lists.is_empty() {
                    continue;
                }
                let (rec, a, data) = lists[rng.below(lists.len() as u64) as usize];
                let mut head = vec![0u8; 512];
                let _ = base.src.read_at(data, &mut head);
                // The entries visible in the first sector.
                let mut offsets = Vec::new();
                let mut pos = 0usize;
                while pos + 0x1A <= head.len() {
                    let len = le16(&head, pos + 4);
                    if len < 0x1A || !len.is_multiple_of(8) {
                        break;
                    }
                    offsets.push(pos);
                    pos += len;
                }
                match rng.below(7) {
                    0 | 1 if !offsets.is_empty() => {
                        let e = *rng.pick(&offsets) as u64;
                        let &(at, w) = rng.pick(LIST_ENTRY);
                        p.patches.push(field(rng, base, data + e + at as u64, w));
                    }
                    2 => p
                        .patches
                        .push(field(rng, base, rec.off + (a.at + 0x30) as u64, 8)),
                    3 => {
                        let at = rec.off
                            + (a.body + rng.below(a.body_len.max(1) as u64) as usize) as u64;
                        p.patches.push((at, vec![rng.next() as u8]));
                    }
                    4 => p.patches.push((data, vec![0; 512])),
                    5 => {
                        let bytes: Vec<u8> = (0..512).map(|_| rng.next() as u8).collect();
                        p.patches.push((data, bytes));
                    }
                    // The stated size, past what any volume could hold and
                    // past the reader's own cap.
                    _ => {
                        let big = [1u64 << 62, (64 << 20) + 1, u64::MAX, 1 << 40];
                        p.patches.push((
                            rec.off + (a.at + 0x30) as u64,
                            rng.pick(&big).to_le_bytes().to_vec(),
                        ));
                    }
                }
            }
            "mft-record" => {
                // Record 0 describes the table itself; so does the extension
                // record of a fragmented one (record 15).
                let targets: Vec<&RecAt> = base
                    .records
                    .iter()
                    .filter(|r| r.number == 0 || (r.number == 15 && r.in_use))
                    .collect();
                let rec = targets[rng.below(targets.len() as u64) as usize];
                match pick_attr(rng, rec, &[0x80, 0x20, 0x30, 0xB0, 0x10]) {
                    Some(a) if a.non_resident && rng.chance(2) => {
                        let at = rec.off
                            + (a.body + rng.below(a.body_len.max(1) as u64) as usize) as u64;
                        p.patches.push((at, vec![rng.next() as u8]));
                    }
                    Some(a) => {
                        let table = if a.non_resident {
                            ATTR_NONRES
                        } else {
                            ATTR_RESIDENT
                        };
                        let &(at, w) = rng.pick(table);
                        p.patches
                            .push(field(rng, base, rec.off + (a.at + at) as u64, w));
                    }
                    None => {
                        let &(at, w) = rng.pick(REC_FIELDS);
                        p.patches.push(field(rng, base, rec.off + at, w));
                    }
                }
            }
            "bit-flips" => {
                let rec = pick_record(rng, base, false);
                for _ in 0..1 + rng.below(24) {
                    let at = rec.off + rng.below(base.record_bytes as u64);
                    let mut b = [0u8];
                    let _ = base.src.read_at(at, &mut b);
                    p.patches.push((at, vec![b[0] ^ (1 << rng.below(8))]));
                }
            }
            "garbage" => {
                let rec = pick_record(rng, base, false);
                match rng.below(4) {
                    0 => {
                        let len = 1 + rng.below(200) as usize;
                        let at = rec.off + rng.below((base.record_bytes - len) as u64);
                        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                        p.patches.push((at, bytes));
                    }
                    1 => p.patches.push((rec.off, vec![0; base.record_bytes])),
                    2 => {
                        // One record's bytes over another's: duplicate
                        // numbers, loops, names pointing at the wrong place.
                        let other = pick_record(rng, base, false);
                        let mut bytes = vec![0u8; base.record_bytes];
                        let _ = base.src.read_at(other.off, &mut bytes);
                        p.patches.push((rec.off, bytes));
                    }
                    _ => {
                        let other = pick_record(rng, base, false);
                        let mut a = vec![0u8; base.record_bytes];
                        let mut b = vec![0u8; base.record_bytes];
                        let _ = base.src.read_at(rec.off, &mut a);
                        let _ = base.src.read_at(other.off, &mut b);
                        p.patches.push((rec.off, b));
                        p.patches.push((other.off, a));
                    }
                }
            }
            "device" => match rng.below(3) {
                0 => {
                    // Cut short.
                    p.len = rng.below(p.len);
                }
                1 => {
                    let rec = pick_record(rng, base, false);
                    let span = base.record_bytes as u64 * (1 + rng.below(40));
                    p.unreadable.push((rec.off, rec.off + span));
                }
                _ => {
                    let start = base.table_start + rng.below(4 << 20);
                    p.unreadable
                        .push((start, start + 512 * (1 + rng.below(16))));
                }
            },
            other => panic!("unknown family {other}"),
        }
    }
    p
}

// ---- running a variant ----

#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub refused: bool,
    pub damaged: bool,
    pub panicked: Option<String>,
    pub timed_out: bool,
    pub invalid_tree: Option<String>,
    pub peak: usize,
    pub elapsed: Duration,
    pub report: Option<NtfsReport>,
    /// The text of the error when the volume was refused.
    pub refusal: Option<String>,
}

pub const TIME_BOUND: Duration = Duration::from_secs(20);

/// `baseline_unreachable` is how many in-use files the clean volume has that
/// no directory reaches; more or fewer is damage.
pub fn run_variant(src: Patched, threads: usize, baseline_unreachable: u64) -> Outcome {
    let options = NtfsOptions {
        threads: Some(threads),
        max_memory_bytes: 512 << 20,
        ..NtfsOptions::default()
    };
    run_variant_with(src, options, baseline_unreachable)
}

/// [`run_variant`] with the options of the caller's choosing.
pub fn run_variant_with(src: Patched, options: NtfsOptions, baseline_unreachable: u64) -> Outcome {
    let scanner = SourceScanner::new(src, options);
    let (tx, rx) = mpsc::channel();
    CURRENT.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    ON.store(true, Ordering::SeqCst);
    let begun = Instant::now();
    let runner = scanner.clone();
    std::thread::spawn(move || {
        let handle = start(runner, "C:\\".into(), ScanOptions::default());
        let result = handle.wait();
        let verified = result.snapshot.verify().err().map(|v| format!("{v:?}"));
        let lost = result.snapshot.lost_records();
        let _ = tx.send((
            result.stats.error,
            (result.stats.records_rejected, lost),
            verified,
        ));
    });
    let got = rx.recv_timeout(TIME_BOUND);
    let elapsed = begun.elapsed();
    ON.store(false, Ordering::SeqCst);
    let peak = PEAK.load(Ordering::Relaxed);
    let mut out = Outcome {
        peak,
        elapsed,
        ..Outcome::default()
    };
    match got {
        Err(_) => out.timed_out = true,
        Ok((error, rejected, verified)) => {
            out.invalid_tree = verified;
            // The tree refuses a record whose size would overflow its sums
            // (a size field the disk made up). That is its defence, and it
            // is fine as long as the loss is on the books.
            let (refused, lost) = rejected;
            if refused != 0 {
                out.damaged = true;
                if lost < refused {
                    out.invalid_tree =
                        Some(format!("{refused} records refused, {lost} counted lost"));
                }
            }
            match error {
                Some(ScanError::Panicked(m)) => out.panicked = Some(m),
                Some(_) => out.refused = true,
                None => {}
            }
            let outcome = scanner.outcome.lock().unwrap().take();
            match outcome {
                Some(Ok(report)) => {
                    out.damaged = out.damaged
                        || report.diagnostics.total() > 0
                        || report.lost_records > 0
                        || report.unreadable_entries > 0
                        || report.adopted.files > 0
                        || report.unreachable.files != baseline_unreachable;
                    out.report = Some(report);
                }
                Some(Err(text)) => out.refusal = Some(text),
                None => {}
            }
        }
    }
    out
}

// ---- clean volumes to corrupt ----

pub fn synthetic_plain() -> Vec<u8> {
    use crate::common::testkit::ROOT;
    let mut v = Volume::new(Spec::default());
    let docs = v.dir("docs", ROOT);
    let deep = v.dir("deep", docs);
    v.file("a.txt", docs, 100);
    let b = v.file("b.bin", docs, 70_000);
    v.link(b, "b-link.bin", deep);
    v.dos_alias(b, "B~1.BIN");
    let s = v.file("streams", ROOT, 20_000);
    v.stream(s, "one", 50);
    v.stream(s, "two", 9_000);
    v.sparse_file("sparse", ROOT, 2, 50, 3);
    v.compressed_file("packed", ROOT, 16, 5);
    v.fragmented_file("frag", deep, 12, 3);
    let junction = v.dir("junction", ROOT);
    v.reparse(junction, 0xA000_0003);
    v.file("inside-junction", junction, 3000);
    let link = v.file("symlink", ROOT, 0);
    v.reparse(link, 0xA000_000C);
    let cloud = v.file("cloud.docx", ROOT, 1000);
    v.set_attrs(cloud, 0x20 | 0x1000);
    let d = v.dir("indexed", ROOT);
    v.index_buffers(d, 3);
    for i in 0..30 {
        v.file(&format!("file-{i:03}"), d, i * 37);
    }
    v.system_file(6, "$Bitmap", 9000);
    v.finish()
}

pub fn synthetic_extensions() -> Vec<u8> {
    use crate::common::testkit::ROOT;
    let mut v = Volume::new(Spec {
        mft_fragments: 5,
        mft_runs_in_extension: true,
        ..Spec::default()
    });
    let d = v.dir("d", ROOT);
    for i in 0..3 {
        let f = v.file(&format!("big{i}"), ROOT, 30_000);
        v.stream(f, "ads", 9_000);
        v.link(f, &format!("other{i}"), d);
        v.with_extension(f, i % 2 == 1);
    }
    for i in 0..60 {
        v.file(&format!("f{i}"), d, i * 11);
    }
    v.finish()
}

pub fn synthetic_deep() -> Vec<u8> {
    use crate::common::testkit::ROOT;
    let mut v = Volume::new(Spec {
        mft_fragments: 3,
        ..Spec::default()
    });
    let mut parent = ROOT;
    for i in 0..70 {
        parent = v.dir(&format!("level{i}"), parent);
        v.file(&format!("leaf{i}"), parent, 500 + i);
    }
    v.finish()
}

// ---- the driver ----

/// The most memory one read may take, over everything the process holds when
/// it starts. The table of a volume this size is a few megabytes.
pub const MEMORY_BOUND: usize = 64 << 20;

#[derive(Default)]
pub struct Tally {
    pub refused: u64,
    pub damaged: u64,
    pub clean: u64,
    pub worst_peak: usize,
    pub slowest: Duration,
}

pub struct Summary {
    pub table: Vec<(String, String, Tally)>,
    pub variants: u64,
}

/// The first seed of every family: `HOSTILE_SEED_BASE` (default 0), so a run
/// can be pointed at seeds nobody has tried.
fn seed_base() -> u64 {
    std::env::var("HOSTILE_SEED_BASE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn seeds_wanted() -> u64 {
    std::env::var("HOSTILE_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120)
}

/// Runs every family over every base, `HOSTILE_SEEDS` seeds each, and checks
/// the reader's promises for every variant. `HOSTILE_ONLY=base:family:seed`
/// runs one case and prints what it found; `HOSTILE_SEED_BASE` starts the
/// seeds elsewhere.
pub fn drive(bases: &[Base]) -> Summary {
    let only: Option<(usize, usize, u64)> = std::env::var("HOSTILE_ONLY").ok().and_then(|s| {
        let parts: Vec<&str> = s.split(':').collect();
        let [base, family, seed] = parts[..] else {
            return None;
        };
        let b = bases.iter().position(|b| b.name == base)?;
        let f = FAMILIES.iter().position(|f| *f == family)?;
        Some((b, f, seed.parse().ok()?))
    });
    let seeds = seeds_wanted();
    let base_seed = seed_base();
    let mut summary = Summary {
        table: Vec::new(),
        variants: 0,
    };
    for (bi, base) in bases.iter().enumerate() {
        for (fi, family) in FAMILIES.iter().enumerate() {
            let mut tally = Tally::default();
            let range = match only {
                Some((b, f, s)) if b == bi && f == fi => s..s + 1,
                Some(_) => 0..0,
                None => base_seed..base_seed.saturating_add(seeds),
            };
            for seed in range {
                let mut rng = Rng::new(seed ^ ((fi as u64) << 32) ^ ((bi as u64) << 48));
                let variant = mutate(family, &mut rng, base);
                let threads = 1 + (seed % 3) as usize;
                let label = format!("{}:{family}:{seed}", base.name);
                let out = run_variant(variant, threads, base.baseline_unreachable);
                if only.is_some() {
                    println!("{label}: {out:#?}");
                }
                assert!(
                    out.panicked.is_none(),
                    "{label}: panicked: {:?}",
                    out.panicked
                );
                assert!(!out.timed_out, "{label}: did not finish in {TIME_BOUND:?}");
                assert!(
                    out.invalid_tree.is_none(),
                    "{label}: the tree is not consistent: {:?}",
                    out.invalid_tree
                );
                assert!(
                    out.peak <= MEMORY_BOUND,
                    "{label}: peak memory {} bytes over the {MEMORY_BOUND} bound",
                    out.peak
                );
                if out.refused {
                    tally.refused += 1;
                } else if out.damaged {
                    tally.damaged += 1;
                } else {
                    tally.clean += 1;
                }
                tally.worst_peak = tally.worst_peak.max(out.peak);
                tally.slowest = tally.slowest.max(out.elapsed);
                summary.variants += 1;
            }
            summary
                .table
                .push((base.name.clone(), (*family).to_string(), tally));
        }
    }
    summary
}

impl Summary {
    /// Prints the outcome per base and family.
    pub fn print(&self) {
        println!("{} variants", self.variants);
        println!(
            "{:<12} {:<16} {:>8} {:>8} {:>8} {:>10} {:>9}",
            "base", "family", "refused", "damaged", "clean", "peak KiB", "slowest"
        );
        for (b, f, t) in &self.table {
            println!(
                "{b:<12} {f:<16} {:>8} {:>8} {:>8} {:>10} {:>9.1?}",
                t.refused,
                t.damaged,
                t.clean,
                t.worst_peak / 1024,
                t.slowest
            );
        }
    }

    /// Every family must have been noticed at least once, or its mutator
    /// does nothing.
    pub fn assert_every_family_bites(&self) {
        for family in FAMILIES {
            let noticed: u64 = self
                .table
                .iter()
                .filter(|(_, f, _)| f == family)
                .map(|(_, _, t)| t.refused + t.damaged)
                .sum();
            let total: u64 = self
                .table
                .iter()
                .filter(|(_, f, _)| f == family)
                .map(|(_, _, t)| t.refused + t.damaged + t.clean)
                .sum();
            if total > 0 {
                assert!(
                    noticed > 0,
                    "family {family}: {total} variants and none was noticed"
                );
            }
        }
    }
}

/// A source that panics when read, to show the harness would see one.
pub struct Panicky;

impl ByteSource for Panicky {
    fn len(&self) -> u64 {
        1 << 20
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> io::Result<()> {
        panic!("a reader bug");
    }
}

pub fn make_base(name: &str, image: Vec<u8>) -> Base {
    survey(name, Arc::new(image))
}
