//! Building NTFS volumes in memory, for tests of the reader here and of the
//! readers' users: file records with correct update sequences, run lists,
//! attribute lists, extension records, a fragmented table, hard links, alternate
//! streams, reparse points. Not part of the reader's interface.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::too_many_arguments,
    dead_code,
    missing_docs
)]

use bs_scan::{ScanContext, ScanEnd, ScanError, ScanOptions, ScanResult, Scanner, start};
use bs_scan_ntfs::{ByteSource, NtfsOptions, NtfsReport, read_volume};
use bs_tree::{NodeFlags, NodeId, NodeKind, NodeRef, Snapshot};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Seconds between 1601-01-01 and 1970-01-01.
const EPOCH_GAP: i64 = 11_644_473_600;

pub fn filetime(unix: i64) -> i64 {
    (unix + EPOCH_GAP) * 10_000_000
}

/// Mapping pairs for runs given as (absolute first cluster or `None` for a
/// hole, length).
pub fn encode_runs(runs: &[(Option<u64>, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev: i64 = 0;
    for &(lcn, len) in runs {
        let len_bytes = minimal_unsigned(len);
        match lcn {
            None => {
                out.push(len_bytes.len() as u8);
                out.extend_from_slice(&len_bytes);
            }
            Some(lcn) => {
                let delta = lcn as i64 - prev;
                prev = lcn as i64;
                let off_bytes = minimal_signed(delta);
                out.push((off_bytes.len() as u8) << 4 | len_bytes.len() as u8);
                out.extend_from_slice(&len_bytes);
                out.extend_from_slice(&off_bytes);
            }
        }
    }
    out.push(0);
    out
}

fn minimal_unsigned(v: u64) -> Vec<u8> {
    let mut b = v.to_le_bytes().to_vec();
    while b.len() > 1 && *b.last().unwrap() == 0 {
        b.pop();
    }
    // A set top bit would read as a negative length.
    if *b.last().unwrap() & 0x80 != 0 {
        b.push(0);
    }
    b
}

fn minimal_signed(v: i64) -> Vec<u8> {
    let mut b = v.to_le_bytes().to_vec();
    while b.len() > 1 {
        let last = b[b.len() - 1];
        let prev = b[b.len() - 2];
        let redundant = (last == 0 && prev & 0x80 == 0) || (last == 0xFF && prev & 0x80 != 0);
        if redundant {
            b.pop();
        } else {
            break;
        }
    }
    b
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// One attribute, as bytes of the record body.
pub fn resident_attr(ty: u32, name: &str, flags: u16, id: u16, value: &[u8]) -> Vec<u8> {
    let name16: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let name_at = 0x18usize;
    let value_at = align8(name_at + name16.len());
    let len = align8(value_at + value.len());
    let mut a = vec![0u8; len];
    a[0..4].copy_from_slice(&ty.to_le_bytes());
    a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    a[8] = 0;
    a[9] = (name16.len() / 2) as u8;
    a[10..12].copy_from_slice(&(name_at as u16).to_le_bytes());
    a[12..14].copy_from_slice(&flags.to_le_bytes());
    a[14..16].copy_from_slice(&id.to_le_bytes());
    a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
    a[0x14..0x16].copy_from_slice(&(value_at as u16).to_le_bytes());
    a[name_at..name_at + name16.len()].copy_from_slice(&name16);
    a[value_at..value_at + value.len()].copy_from_slice(value);
    a
}

#[derive(Clone, Debug)]
pub struct NonRes {
    pub ty: u32,
    pub name: String,
    pub flags: u16,
    pub lowest_vcn: u64,
    pub highest_vcn: u64,
    pub unit: u8,
    pub allocated: u64,
    pub size: u64,
    pub initialized: u64,
    pub compressed: Option<u64>,
    pub runs: Vec<u8>,
}

pub fn nonresident_attr(id: u16, n: &NonRes) -> Vec<u8> {
    let name16: Vec<u8> = n.name.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let header = if n.compressed.is_some() { 0x48 } else { 0x40 };
    let name_at = header;
    let runs_at = align8(name_at + name16.len());
    let len = align8(runs_at + n.runs.len());
    let mut a = vec![0u8; len];
    a[0..4].copy_from_slice(&n.ty.to_le_bytes());
    a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    a[8] = 1;
    a[9] = (name16.len() / 2) as u8;
    a[10..12].copy_from_slice(&(name_at as u16).to_le_bytes());
    a[12..14].copy_from_slice(&n.flags.to_le_bytes());
    a[14..16].copy_from_slice(&id.to_le_bytes());
    a[0x10..0x18].copy_from_slice(&n.lowest_vcn.to_le_bytes());
    a[0x18..0x20].copy_from_slice(&n.highest_vcn.to_le_bytes());
    a[0x20..0x22].copy_from_slice(&(runs_at as u16).to_le_bytes());
    a[0x22] = n.unit;
    a[0x28..0x30].copy_from_slice(&n.allocated.to_le_bytes());
    a[0x30..0x38].copy_from_slice(&n.size.to_le_bytes());
    a[0x38..0x40].copy_from_slice(&n.initialized.to_le_bytes());
    if let Some(c) = n.compressed {
        a[0x40..0x48].copy_from_slice(&c.to_le_bytes());
    }
    a[name_at..name_at + name16.len()].copy_from_slice(&name16);
    a[runs_at..runs_at + n.runs.len()].copy_from_slice(&n.runs);
    a
}

pub fn standard_info_value(attrs: u32, mtime_unix: i64) -> Vec<u8> {
    let mut v = vec![0u8; 0x48];
    let t = filetime(mtime_unix).to_le_bytes();
    for at in [0usize, 8, 16, 24] {
        v[at..at + 8].copy_from_slice(&t);
    }
    v[32..36].copy_from_slice(&attrs.to_le_bytes());
    v
}

pub fn file_name_value(parent_rec: u64, parent_seq: u16, ns: u8, units: &[u16]) -> Vec<u8> {
    let mut v = vec![0u8; 0x42 + units.len() * 2];
    let parent = parent_rec | (u64::from(parent_seq) << 48);
    v[0..8].copy_from_slice(&parent.to_le_bytes());
    v[0x40] = units.len() as u8;
    v[0x41] = ns;
    for (i, u) in units.iter().enumerate() {
        v[0x42 + 2 * i..0x44 + 2 * i].copy_from_slice(&u.to_le_bytes());
    }
    v
}

/// Builds one file record of `size` bytes.
#[derive(Clone, Debug)]
pub struct RecordBuilder {
    size: usize,
    seq: u16,
    flags: u16,
    base: u64,
    links: u16,
    attrs: Vec<Vec<u8>>,
    next_id: u16,
    number: u32,
}

/// A record's own number, as `RecordBuilder` leaves it until the volume
/// puts the record in its slot.
pub const NUMBER_UNSET: u32 = u32::MAX;

impl RecordBuilder {
    pub fn new(size: usize, seq: u16) -> RecordBuilder {
        RecordBuilder {
            size,
            seq,
            flags: 1,
            base: 0,
            links: 1,
            attrs: Vec::new(),
            next_id: 0,
            number: NUMBER_UNSET,
        }
    }

    /// Writes `n` as the record's own number, whatever slot it goes in.
    pub fn number(mut self, n: u32) -> Self {
        self.number = n;
        self
    }

    pub fn directory(mut self) -> Self {
        self.flags |= 2;
        self
    }

    pub fn unused(mut self) -> Self {
        self.flags &= !1;
        self
    }

    pub fn extension_of(mut self, base_rec: u64, base_seq: u16) -> Self {
        self.base = base_rec | (u64::from(base_seq) << 48);
        self
    }

    pub fn links(mut self, n: u16) -> Self {
        self.links = n;
        self
    }

    fn id(&mut self) -> u16 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn raw(mut self, attr: Vec<u8>) -> Self {
        self.attrs.push(attr);
        self
    }

    pub fn resident(mut self, ty: u32, name: &str, value: &[u8]) -> Self {
        let id = self.id();
        self.attrs.push(resident_attr(ty, name, 0, id, value));
        self
    }

    pub fn standard_info(self, attrs: u32, mtime: i64) -> Self {
        self.resident(0x10, "", &standard_info_value(attrs, mtime))
    }

    pub fn file_name(self, parent: u64, parent_seq: u16, ns: u8, name: &str) -> Self {
        let units: Vec<u16> = name.encode_utf16().collect();
        self.resident(0x30, "", &file_name_value(parent, parent_seq, ns, &units))
    }

    pub fn file_name_units(self, parent: u64, parent_seq: u16, ns: u8, units: &[u16]) -> Self {
        self.resident(0x30, "", &file_name_value(parent, parent_seq, ns, units))
    }

    pub fn data_resident(self, name: &str, bytes: &[u8]) -> Self {
        self.resident(0x80, name, bytes)
    }

    pub fn nonres(mut self, n: &NonRes) -> Self {
        let id = self.id();
        self.attrs.push(nonresident_attr(id, n));
        self
    }

    /// Lays the attributes out, ends them, and stamps the update sequence.
    pub fn build(self) -> Vec<u8> {
        let usa_at = 0x30usize;
        let usa_count = self.size / 512 + 1;
        let attrs_at = align8(usa_at + usa_count * 2);
        let mut rec = vec![0u8; self.size];
        rec[0..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&(usa_at as u16).to_le_bytes());
        rec[6..8].copy_from_slice(&(usa_count as u16).to_le_bytes());
        rec[0x10..0x12].copy_from_slice(&self.seq.to_le_bytes());
        rec[0x12..0x14].copy_from_slice(&self.links.to_le_bytes());
        rec[0x14..0x16].copy_from_slice(&(attrs_at as u16).to_le_bytes());
        rec[0x16..0x18].copy_from_slice(&self.flags.to_le_bytes());
        rec[0x1C..0x20].copy_from_slice(&(self.size as u32).to_le_bytes());
        rec[0x20..0x28].copy_from_slice(&self.base.to_le_bytes());
        rec[0x28..0x2A].copy_from_slice(&self.next_id.to_le_bytes());
        rec[0x2C..0x30].copy_from_slice(&self.number.to_le_bytes());
        let mut at = attrs_at;
        for a in &self.attrs {
            assert!(
                at + a.len() + 8 <= self.size,
                "the attributes do not fit a record of {} bytes",
                self.size
            );
            rec[at..at + a.len()].copy_from_slice(a);
            at += a.len();
        }
        rec[at..at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        at += 8;
        rec[0x18..0x1C].copy_from_slice(&(at as u32).to_le_bytes());
        stamp(&mut rec);
        rec
    }
}

/// Writes the update sequence number over the end of every sector, saving
/// what was there.
pub fn stamp(rec: &mut [u8]) {
    let usa_at = u16::from_le_bytes([rec[4], rec[5]]) as usize;
    let usa_count = u16::from_le_bytes([rec[6], rec[7]]) as usize;
    let usn: u16 = 0x55AA ^ 0x0F0F;
    rec[usa_at..usa_at + 2].copy_from_slice(&usn.to_le_bytes());
    for i in 1..usa_count {
        let tail = i * 512 - 2;
        let saved = [rec[tail], rec[tail + 1]];
        rec[usa_at + 2 * i..usa_at + 2 * i + 2].copy_from_slice(&saved);
        rec[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
    }
}

/// Undoes [`stamp`], returning the plain record (for tests that edit fields
/// and stamp again).
pub fn unstamp(rec: &mut [u8]) {
    let usa_at = u16::from_le_bytes([rec[4], rec[5]]) as usize;
    let usa_count = u16::from_le_bytes([rec[6], rec[7]]) as usize;
    for i in 1..usa_count {
        let tail = i * 512 - 2;
        let saved = [rec[usa_at + 2 * i], rec[usa_at + 2 * i + 1]];
        rec[tail..tail + 2].copy_from_slice(&saved);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub cluster_bytes: u32,
    pub record_bytes: u32,
    pub clusters: u64,
    /// Clusters set aside for the table; it never grows past them.
    pub mft_area_clusters: u64,
    /// The table's data is split into this many runs, placed out of order.
    pub mft_fragments: usize,
    /// The runs after the first are kept in an extension record of the table.
    pub mft_runs_in_extension: bool,
}

impl Default for Spec {
    fn default() -> Self {
        Spec {
            cluster_bytes: 4096,
            record_bytes: 1024,
            clusters: 4096,
            mft_area_clusters: 1024,
            mft_fragments: 1,
            mft_runs_in_extension: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NameSpec {
    pub parent: u64,
    pub parent_seq: u16,
    pub ns: u8,
    pub units: Vec<u16>,
}

#[derive(Clone, Debug)]
pub struct Stream {
    pub name: String,
    pub size: u64,
    /// Resident when `runs` is empty and the size is small.
    pub runs: Vec<(Option<u64>, u64)>,
    pub flags: u16,
    pub unit: u8,
    /// A compressed stream's packed size, when it is recorded.
    pub compressed: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Ent {
    pub rec: u32,
    pub seq: u16,
    pub dir: bool,
    pub names: Vec<NameSpec>,
    pub attrs: u32,
    pub mtime: i64,
    pub streams: Vec<Stream>,
    pub reparse: Option<u32>,
    /// Clusters of index buffers (non-resident `$INDEX_ALLOCATION`).
    pub index_clusters: Option<(u64, u64)>,
    /// Keep the data, names and reparse data in an extension record, behind
    /// an attribute list.
    pub extension: bool,
    /// Store the attribute list in clusters instead of in the record.
    pub list_in_clusters: bool,
    /// The file has extended attributes.
    pub ea: bool,
}

pub struct Volume {
    pub spec: Spec,
    pub ents: BTreeMap<u32, Ent>,
    next_rec: u32,
    next_cluster: u64,
    /// Extra raw records, by number, written as given.
    pub raw: BTreeMap<u32, Vec<u8>>,
    blobs: Vec<(u64, Vec<u8>)>,
    pub serial: u64,
}

pub const ROOT: u32 = 5;

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

impl Volume {
    pub fn new(spec: Spec) -> Volume {
        let mut v = Volume {
            spec,
            ents: BTreeMap::new(),
            next_rec: 24,
            next_cluster: 4 + spec.mft_area_clusters + 8,
            raw: BTreeMap::new(),
            blobs: Vec::new(),
            serial: 0x1234_5678_9ABC_DEF0,
        };
        v.ents.insert(
            ROOT,
            Ent {
                rec: ROOT,
                seq: 5,
                dir: true,
                names: vec![NameSpec {
                    parent: u64::from(ROOT),
                    parent_seq: 5,
                    ns: 3,
                    units: units("."),
                }],
                attrs: 0x1000_0006,
                mtime: 1_700_000_000,
                streams: Vec::new(),
                reparse: None,
                index_clusters: None,
                extension: false,
                list_in_clusters: false,
                ea: false,
            },
        );
        v
    }

    /// Takes `n` clusters for data and returns the first.
    pub fn alloc(&mut self, n: u64) -> u64 {
        let at = self.next_cluster;
        self.next_cluster += n;
        assert!(
            self.next_cluster <= self.spec.clusters,
            "the test volume is full"
        );
        at
    }

    fn clusters_for(&self, size: u64) -> u64 {
        size.div_ceil(u64::from(self.spec.cluster_bytes))
    }

    fn add(&mut self, mut ent: Ent) -> u32 {
        let rec = self.next_rec;
        self.next_rec += 1;
        ent.rec = rec;
        self.ents.insert(rec, ent);
        rec
    }

    fn base_ent(&self, name: &str, parent: u32, dir: bool, mtime: i64) -> Ent {
        let parent_seq = self.ents.get(&parent).map_or(1, |e| e.seq);
        Ent {
            rec: 0,
            seq: 1,
            dir,
            names: vec![NameSpec {
                parent: u64::from(parent),
                parent_seq,
                ns: 3,
                units: units(name),
            }],
            attrs: if dir { 0x1000_0000 } else { 0x20 },
            mtime,
            streams: Vec::new(),
            reparse: None,
            index_clusters: None,
            extension: false,
            list_in_clusters: false,
            ea: false,
        }
    }

    pub fn dir(&mut self, name: &str, parent: u32) -> u32 {
        let e = self.base_ent(name, parent, true, 1_700_000_100);
        self.add(e)
    }

    /// A plain file of `size` bytes: in the record when small, else in
    /// contiguous clusters.
    pub fn file(&mut self, name: &str, parent: u32, size: u64) -> u32 {
        let stream = self.plain_stream("", size);
        let mut e = self.base_ent(name, parent, false, 1_700_000_200);
        e.streams.push(stream);
        self.add(e)
    }

    pub fn plain_stream(&mut self, name: &str, size: u64) -> Stream {
        if size <= 600 {
            Stream {
                name: name.into(),
                size,
                runs: Vec::new(),
                flags: 0,
                unit: 0,
                compressed: None,
            }
        } else {
            let n = self.clusters_for(size);
            let lcn = self.alloc(n);
            Stream {
                name: name.into(),
                size,
                runs: vec![(Some(lcn), n)],
                flags: 0,
                unit: 0,
                compressed: None,
            }
        }
    }

    /// Adds an alternate data stream to `rec`.
    pub fn stream(&mut self, rec: u32, name: &str, size: u64) {
        let s = self.plain_stream(name, size);
        self.ents.get_mut(&rec).unwrap().streams.push(s);
    }

    /// A sparse file: `data` clusters of data, then a hole, then `tail`
    /// clusters of data.
    pub fn sparse_file(&mut self, name: &str, parent: u32, data: u64, hole: u64, tail: u64) -> u32 {
        let a = self.alloc(data);
        let b = self.alloc(tail);
        let total = data + hole + tail;
        let mut e = self.base_ent(name, parent, false, 1_700_000_300);
        e.attrs |= 0x200;
        e.streams.push(Stream {
            name: String::new(),
            size: total * u64::from(self.spec.cluster_bytes) - 100,
            runs: vec![(Some(a), data), (None, hole), (Some(b), tail)],
            flags: 0x8000,
            unit: 4,
            compressed: Some((data + tail) * u64::from(self.spec.cluster_bytes)),
        });
        self.add(e)
    }

    /// A compressed file whose `clusters` clusters of logical size occupy
    /// `packed` clusters.
    pub fn compressed_file(&mut self, name: &str, parent: u32, clusters: u64, packed: u64) -> u32 {
        let a = self.alloc(packed);
        let mut e = self.base_ent(name, parent, false, 1_700_000_400);
        e.attrs |= 0x800;
        e.streams.push(Stream {
            name: String::new(),
            size: clusters * u64::from(self.spec.cluster_bytes) - 7,
            runs: vec![(Some(a), packed), (None, clusters - packed)],
            flags: 0x0001,
            unit: 4,
            compressed: Some(packed * u64::from(self.spec.cluster_bytes)),
        });
        self.add(e)
    }

    /// A file whose data is in `fragments` separate runs.
    pub fn fragmented_file(
        &mut self,
        name: &str,
        parent: u32,
        clusters: u64,
        fragments: u64,
    ) -> u32 {
        let each = clusters / fragments;
        let mut runs = Vec::new();
        let mut left = clusters;
        for i in 0..fragments {
            let n = if i + 1 == fragments { left } else { each };
            left -= n;
            // Leave a one-cluster gap so the runs are not adjacent.
            let lcn = self.alloc(n + 1);
            runs.push((Some(lcn), n));
        }
        let mut e = self.base_ent(name, parent, false, 1_700_000_500);
        e.streams.push(Stream {
            name: String::new(),
            size: clusters * u64::from(self.spec.cluster_bytes),
            runs,
            flags: 0,
            unit: 0,
            compressed: None,
        });
        self.add(e)
    }

    /// Another name for `rec`.
    pub fn link(&mut self, rec: u32, name: &str, parent: u32) {
        let parent_seq = self.ents.get(&parent).map_or(1, |e| e.seq);
        self.ents.get_mut(&rec).unwrap().names.push(NameSpec {
            parent: u64::from(parent),
            parent_seq,
            ns: 3,
            units: units(name),
        });
    }

    /// Gives `rec`'s first name a separate DOS alias in the same directory.
    pub fn dos_alias(&mut self, rec: u32, alias: &str) {
        let e = self.ents.get_mut(&rec).unwrap();
        let first = e.names[0].clone();
        e.names[0].ns = 1;
        e.names.push(NameSpec {
            ns: 2,
            units: units(alias),
            ..first
        });
    }

    pub fn set_attrs(&mut self, rec: u32, attrs: u32) {
        self.ents.get_mut(&rec).unwrap().attrs = attrs;
    }

    pub fn set_mtime(&mut self, rec: u32, mtime: i64) {
        self.ents.get_mut(&rec).unwrap().mtime = mtime;
    }

    /// Makes `rec` a reparse point with `tag`.
    pub fn reparse(&mut self, rec: u32, tag: u32) {
        let e = self.ents.get_mut(&rec).unwrap();
        e.reparse = Some(tag);
        e.attrs |= 0x400;
    }

    /// Gives a directory `clusters` clusters of index buffers.
    pub fn index_buffers(&mut self, rec: u32, clusters: u64) {
        let lcn = self.alloc(clusters);
        self.ents.get_mut(&rec).unwrap().index_clusters = Some((lcn, clusters));
    }

    /// Moves the data attributes and names after the first into an extension
    /// record behind an attribute list.
    pub fn with_extension(&mut self, rec: u32, list_in_clusters: bool) {
        let e = self.ents.get_mut(&rec).unwrap();
        e.extension = true;
        e.list_in_clusters = list_in_clusters;
    }

    /// Gives `rec` extended attributes (an `$EA_INFORMATION` and an `$EA`).
    pub fn extended_attributes(&mut self, rec: u32) {
        self.ents.get_mut(&rec).unwrap().ea = true;
    }

    pub fn ent_mut(&mut self, rec: u32) -> &mut Ent {
        self.ents.get_mut(&rec).unwrap()
    }

    /// A system file in the root, like `$Bitmap`.
    pub fn system_file(&mut self, rec: u32, name: &str, size: u64) {
        let stream = self.plain_stream("", size);
        let mut e = self.base_ent(name, ROOT, false, 1_700_000_000);
        e.attrs = 0x6;
        e.rec = rec;
        e.seq = rec as u16;
        e.streams.push(stream);
        self.ents.insert(rec, e);
    }

    fn render_stream(&self, s: &Stream, id_hint: u16) -> Vec<u8> {
        let cb = u64::from(self.spec.cluster_bytes);
        if s.runs.is_empty() {
            return resident_attr(0x80, &s.name, 0, id_hint, &vec![0xAB; s.size as usize]);
        }
        let covered: u64 = s.runs.iter().map(|r| r.1).sum();
        let backed: u64 = s.runs.iter().filter(|r| r.0.is_some()).map(|r| r.1).sum();
        let n = NonRes {
            ty: 0x80,
            name: s.name.clone(),
            flags: s.flags,
            lowest_vcn: 0,
            highest_vcn: covered - 1,
            unit: s.unit,
            allocated: if s.unit != 0 {
                covered * cb
            } else {
                backed * cb
            },
            size: s.size,
            initialized: s.size,
            compressed: s.compressed,
            runs: encode_runs(&s.runs),
        };
        nonresident_attr(id_hint, &n)
    }

    /// Renders the record(s) of `e`: the base record, and the extension
    /// record when it has one (at `ext_rec`).
    fn render(&mut self, e: &Ent, ext_rec: Option<u32>) -> Vec<(u32, Vec<u8>)> {
        let rb = self.spec.record_bytes as usize;
        let mut base = RecordBuilder::new(rb, e.seq);
        if e.dir {
            base = base.directory();
        }
        base = base.links(e.names.iter().filter(|n| n.ns != 2).count() as u16);
        base = base.resident(0x10, "", &standard_info_value(e.attrs, e.mtime));
        let mut moved: Vec<Vec<u8>> = Vec::new();
        let mut id = 1u16;
        let mut entries: Vec<(u32, String, u64, u16, bool)> = Vec::new(); // type, name, vcn, id, in_ext
        entries.push((0x10, String::new(), 0, 0, false));
        let name_attr = |n: &NameSpec, id: u16| {
            resident_attr(
                0x30,
                "",
                0,
                id,
                &file_name_value(n.parent, n.parent_seq, n.ns, &n.units),
            )
        };
        for (k, n) in e.names.iter().enumerate() {
            let in_ext = e.extension && k > 0;
            let a = name_attr(n, id);
            if in_ext {
                moved.push(a);
            } else {
                base = base.raw(a);
            }
            entries.push((0x30, String::new(), 0, id, in_ext));
            id += 1;
        }
        if e.dir {
            base = base.raw(resident_attr(0x90, "$I30", 0, id, &[0u8; 32]));
            entries.push((0x90, "$I30".into(), 0, id, false));
            id += 1;
            if let Some((lcn, n)) = e.index_clusters {
                let cb = u64::from(self.spec.cluster_bytes);
                let a = nonresident_attr(
                    id,
                    &NonRes {
                        ty: 0xA0,
                        name: "$I30".into(),
                        flags: 0,
                        lowest_vcn: 0,
                        highest_vcn: n - 1,
                        unit: 0,
                        allocated: n * cb,
                        size: n * cb,
                        initialized: n * cb,
                        compressed: None,
                        runs: encode_runs(&[(Some(lcn), n)]),
                    },
                );
                base = base.raw(a);
                entries.push((0xA0, "$I30".into(), 0, id, false));
                id += 1;
            }
        }
        for s in &e.streams {
            let a = self.render_stream(s, id);
            if e.extension {
                moved.push(a);
            } else {
                base = base.raw(a);
            }
            entries.push((0x80, s.name.clone(), 0, id, e.extension));
            id += 1;
        }
        if e.ea {
            base = base.raw(resident_attr(0xD0, "", 0, id, &[0u8; 8]));
            entries.push((0xD0, String::new(), 0, id, false));
            id += 1;
            base = base.raw(resident_attr(0xE0, "", 0, id, &[0u8; 16]));
            entries.push((0xE0, String::new(), 0, id, false));
            id += 1;
        }
        if let Some(tag) = e.reparse {
            let mut v = vec![0u8; 8];
            v[0..4].copy_from_slice(&tag.to_le_bytes());
            let a = resident_attr(0xC0, "", 0, id, &v);
            if e.extension {
                moved.push(a);
            } else {
                base = base.raw(a);
            }
            entries.push((0xC0, String::new(), 0, id, e.extension));
            id += 1;
        }
        let _ = id;
        let mut out = Vec::new();
        if let Some(ext_no) = ext_rec {
            // The attribute list names every attribute and where it is.
            let mut list = Vec::new();
            for (ty, name, vcn, aid, in_ext) in &entries {
                let name16: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
                let len = align8(0x1A + name16.len());
                let mut ent = vec![0u8; len];
                ent[0..4].copy_from_slice(&ty.to_le_bytes());
                ent[4..6].copy_from_slice(&(len as u16).to_le_bytes());
                ent[6] = (name16.len() / 2) as u8;
                ent[7] = 0x1A;
                ent[8..16].copy_from_slice(&vcn.to_le_bytes());
                let target = if *in_ext {
                    u64::from(ext_no) | (1u64 << 48)
                } else {
                    u64::from(e.rec) | (u64::from(e.seq) << 48)
                };
                ent[16..24].copy_from_slice(&target.to_le_bytes());
                ent[24..26].copy_from_slice(&aid.to_le_bytes());
                ent[0x1A..0x1A + name16.len()].copy_from_slice(&name16);
                list.extend(ent);
            }
            let list_attr = if e.list_in_clusters {
                let n = self.clusters_for(list.len() as u64).max(1);
                let lcn = self.alloc(n);
                let size = list.len() as u64;
                self.blobs.push((lcn, list));
                nonresident_attr(
                    id,
                    &NonRes {
                        ty: 0x20,
                        name: String::new(),
                        flags: 0,
                        lowest_vcn: 0,
                        highest_vcn: n - 1,
                        unit: 0,
                        allocated: n * u64::from(self.spec.cluster_bytes),
                        size,
                        initialized: size,
                        compressed: None,
                        runs: encode_runs(&[(Some(lcn), n)]),
                    },
                )
            } else {
                resident_attr(0x20, "", 0, id, &list)
            };
            base = base.raw(list_attr);
            let mut ext = RecordBuilder::new(rb, 1).extension_of(u64::from(e.rec), e.seq);
            for a in moved {
                ext = ext.raw(a);
            }
            out.push((ext_no, ext.build()));
        }
        out.push((e.rec, base.build()));
        out
    }

    /// Lays the volume out and returns the image (the volume and one sector
    /// after it, as a device that holds it would be).
    pub fn finish(mut self) -> Vec<u8> {
        let cb = u64::from(self.spec.cluster_bytes);
        let rb = u64::from(self.spec.record_bytes);
        let mut image = vec![0u8; (self.spec.clusters * cb + 512) as usize];

        // Records.
        let mut records: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        let ents: Vec<Ent> = self.ents.values().cloned().collect();
        let mut next_ext = 16u32;
        let mut ext_for: BTreeMap<u32, u32> = BTreeMap::new();
        for e in &ents {
            if e.extension {
                // Extension records after the highest numbered file.
                let n = self.next_rec.max(next_ext) + ext_for.len() as u32;
                ext_for.insert(e.rec, n);
            }
        }
        next_ext += 0;
        let _ = next_ext;
        let mft_ext_rec = if self.spec.mft_runs_in_extension {
            Some(15u32)
        } else {
            None
        };
        for e in &ents {
            for (no, bytes) in self.render(e, ext_for.get(&e.rec).copied()) {
                records.insert(no, bytes);
            }
        }
        for (no, bytes) in std::mem::take(&mut self.raw) {
            records.insert(no, bytes);
        }
        // The table's own record.
        let top = records.keys().copied().max().unwrap_or(0).max(ROOT) + 1;
        let table_clusters = (u64::from(top) * rb).div_ceil(cb).max(1);
        assert!(
            table_clusters <= self.spec.mft_area_clusters,
            "{table_clusters} clusters of table do not fit the reserved area"
        );
        let frags = (self.spec.mft_fragments as u64).clamp(1, table_clusters);
        let each = table_clusters / frags;
        let mut runs: Vec<(Option<u64>, u64)> = Vec::new();
        let mut placed = Vec::new();
        let mut left = table_clusters;
        for i in 0..frags {
            let n = if i + 1 == frags { left } else { each };
            left -= n;
            // The first piece is where the boot sector says the table
            // starts; the others lie after it, in reverse order of position.
            placed.push(n);
        }
        let area = 4u64;
        let mut lcns = Vec::new();
        if frags == 1 {
            lcns.push(area);
        } else {
            // First run at the area start; later ones from the far end.
            lcns.push(area);
            let mut at = area + self.spec.mft_area_clusters;
            for n in placed.iter().skip(1) {
                at -= n + 1;
                lcns.push(at);
            }
        }
        for (n, lcn) in placed.iter().zip(&lcns) {
            runs.push((Some(*lcn), *n));
        }
        let size = u64::from(top) * rb;
        let table_alloc = table_clusters * cb;
        let mut rec0 = RecordBuilder::new(rb as usize, 1);
        rec0 = rec0.resident(0x10, "", &standard_info_value(0x6, 1_700_000_000));
        rec0 = rec0.raw(resident_attr(
            0x30,
            "",
            0,
            1,
            &file_name_value(u64::from(ROOT), 5, 3, &units("$MFT")),
        ));
        let (head_runs, ext_runs): (Vec<_>, Vec<_>) = if mft_ext_rec.is_some() && runs.len() > 1 {
            (runs[..1].to_vec(), runs[1..].to_vec())
        } else {
            (runs.clone(), Vec::new())
        };
        let head_cov: u64 = head_runs.iter().map(|r| r.1).sum();
        rec0 = rec0.nonres(&NonRes {
            ty: 0x80,
            name: String::new(),
            flags: 0,
            lowest_vcn: 0,
            highest_vcn: if ext_runs.is_empty() {
                table_clusters - 1
            } else {
                head_cov - 1
            },
            unit: 0,
            allocated: table_alloc,
            size,
            initialized: size,
            compressed: None,
            runs: encode_runs(&head_runs),
        });
        if let (Some(ext_no), false) = (mft_ext_rec, ext_runs.is_empty()) {
            // Mapping pairs of the later runs are relative to the first
            // piece's last run, so they are encoded on their own with an
            // absolute start and joined back by the reader.
            let mut tail = encode_runs(&ext_runs);
            // `encode_runs` starts from cluster 0; the decoder of a piece also
            // starts from 0, which is how real tables store later pieces.
            let _ = &mut tail;
            let mut list = Vec::new();
            let mut entry = |ty: u32, vcn: u64, target: u64, aid: u16| {
                let mut ent = vec![0u8; 0x20];
                ent[0..4].copy_from_slice(&ty.to_le_bytes());
                ent[4..6].copy_from_slice(&0x20u16.to_le_bytes());
                ent[7] = 0x1A;
                ent[8..16].copy_from_slice(&vcn.to_le_bytes());
                ent[16..24].copy_from_slice(&target.to_le_bytes());
                ent[24..26].copy_from_slice(&aid.to_le_bytes());
                list.extend(ent);
            };
            entry(0x10, 0, 1u64 << 48, 0);
            entry(0x30, 0, 1u64 << 48, 1);
            entry(0x80, 0, 1u64 << 48, 2);
            entry(0x80, head_cov, u64::from(ext_no) | (1u64 << 48), 0);
            rec0 = rec0.raw(resident_attr(0x20, "", 0, 9, &list));
            let ext = RecordBuilder::new(rb as usize, 1)
                .extension_of(0, 1)
                .nonres(&NonRes {
                    ty: 0x80,
                    name: String::new(),
                    flags: 0,
                    lowest_vcn: head_cov,
                    highest_vcn: table_clusters - 1,
                    unit: 0,
                    allocated: 0,
                    size: 0,
                    initialized: 0,
                    compressed: None,
                    runs: tail,
                })
                .build();
            records.insert(ext_no, ext);
        }
        records.insert(0, rec0.build());

        // Where each record goes: the table is a stream over its runs.
        let mut table = vec![0u8; (table_clusters * cb) as usize];
        for (no, bytes) in &records {
            let at = *no as usize * rb as usize;
            let slot = &mut table[at..at + rb as usize];
            slot.copy_from_slice(bytes);
            // A record that was not given a number takes its slot's.
            if slot[0x2C..0x30] == NUMBER_UNSET.to_le_bytes() {
                slot[0x2C..0x30].copy_from_slice(&no.to_le_bytes());
            }
        }
        let mut off = 0usize;
        for (n, lcn) in placed.iter().zip(&lcns) {
            let bytes = (n * cb) as usize;
            let dst = (lcn * cb) as usize;
            image[dst..dst + bytes].copy_from_slice(&table[off..off + bytes]);
            off += bytes;
        }
        for (lcn, bytes) in &self.blobs {
            let dst = (*lcn * cb) as usize;
            image[dst..dst + bytes.len()].copy_from_slice(bytes);
        }

        // The boot sector, and its backup after the volume.
        let spc = (cb / 512) as u8;
        let mut boot = vec![0u8; 512];
        boot[3..11].copy_from_slice(b"NTFS    ");
        boot[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        boot[0x0D] = spc;
        boot[0x28..0x30].copy_from_slice(&(self.spec.clusters * cb / 512).to_le_bytes());
        boot[0x30..0x38].copy_from_slice(&area.to_le_bytes());
        boot[0x38..0x40].copy_from_slice(&(self.spec.clusters / 2).to_le_bytes());
        boot[0x40] = (256 - rb.trailing_zeros() as i32) as u8;
        boot[0x48..0x50].copy_from_slice(&self.serial.to_le_bytes());
        boot[510] = 0x55;
        boot[511] = 0xAA;
        image[..512].copy_from_slice(&boot);
        let end = image.len() - 512;
        image[end..].copy_from_slice(&boot);
        image
    }
}

/// A scanner that reads an NTFS image held in memory.
pub struct MemScanner {
    pub image: Arc<Vec<u8>>,
    pub options: NtfsOptions,
    pub report: Mutex<Option<NtfsReport>>,
}

impl MemScanner {
    pub fn new(image: Arc<Vec<u8>>, options: NtfsOptions) -> Arc<MemScanner> {
        Arc::new(MemScanner {
            image,
            options,
            report: Mutex::new(None),
        })
    }
}

impl Scanner for MemScanner {
    fn name(&self) -> &'static str {
        "ntfs-memory"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        match read_volume(
            &**self.image,
            root.to_string_lossy().as_bytes(),
            &self.options,
            ctx,
        ) {
            Ok((end, report)) => {
                *self.report.lock().unwrap() = Some(report);
                Ok(end)
            }
            Err(e) => Err(ScanError::Root(std::io::Error::other(e))),
        }
    }
}

/// A scanner over any byte source, keeping the report or the error text.
pub struct SourceScanner<S: ByteSource + 'static> {
    pub source: S,
    pub options: NtfsOptions,
    pub outcome: Mutex<Option<Result<NtfsReport, String>>>,
}

impl<S: ByteSource + 'static> SourceScanner<S> {
    pub fn new(source: S, options: NtfsOptions) -> Arc<Self> {
        Arc::new(SourceScanner {
            source,
            options,
            outcome: Mutex::new(None),
        })
    }
}

impl<S: ByteSource + 'static> Scanner for SourceScanner<S> {
    fn name(&self) -> &'static str {
        "ntfs-source"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        match read_volume(
            &self.source,
            root.to_string_lossy().as_bytes(),
            &self.options,
            ctx,
        ) {
            Ok((end, report)) => {
                *self.outcome.lock().unwrap() = Some(Ok(report));
                Ok(end)
            }
            Err(e) => {
                *self.outcome.lock().unwrap() = Some(Err(e.to_string()));
                Err(ScanError::Root(std::io::Error::other(e)))
            }
        }
    }
}

/// Reads `image` into a tree.
pub fn scan_memory(image: Vec<u8>, options: NtfsOptions) -> (ScanResult, Option<NtfsReport>) {
    let scanner = MemScanner::new(Arc::new(image), options);
    let handle = start(
        scanner.clone(),
        PathBuf::from("C:\\"),
        ScanOptions::default(),
    );
    let result = handle.wait();
    let report = scanner.report.lock().unwrap().take();
    (result, report)
}

/// What a node is, as far as comparisons go.
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

/// Every node below the root, by path relative to it (`/` between parts).
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

/// The same, with paths as text, for assertions.
pub fn flat(snap: &Snapshot) -> BTreeMap<String, Info> {
    flatten(snap)
        .into_iter()
        .map(|(k, v)| (String::from_utf8_lossy(&k).into_owned(), v))
        .collect()
}
