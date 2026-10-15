//! Reading one file record into the facts the tree needs: names, sizes,
//! attributes, times. The parser keeps what it can from a record that is
//! damaged partway, and says so.

use crate::boot::Geometry;
use crate::budget::Charge;
use crate::error::{Diagnostics, ProblemKind};
use crate::le::{bytes_at, i64_at, u8_at, u16_at, u32_at, u64_at};
use crate::record::{
    ATTR_COMPRESSED, ATTR_SPARSE, Attr, Body, Peek, RecordError, attrs, fix_and_parse, peek,
    split_ref, ty,
};
use crate::runlist;
use bs_scan::winrules::{filetime_to_unix, wtf8_from_utf16};

/// "No name" in a name chain.
pub(crate) const NONE: u32 = u32::MAX;

/// Flags of a [`FileRec`].
pub(crate) const F_PRESENT: u8 = 0x01;
pub(crate) const F_DIR: u8 = 0x02;
pub(crate) const F_HAS_LIST: u8 = 0x04;
/// Part of the file could not be read or is missing; its sizes are a lower
/// bound.
pub(crate) const F_DAMAGED: u8 = 0x08;
/// The unnamed data stream is marked compressed or sparse.
pub(crate) const F_COMPRESSED_OR_SPARSE: u8 = 0x10;
/// The unnamed data stream lives inside the file record.
pub(crate) const F_RESIDENT_DATA: u8 = 0x20;
/// The file has extended attributes. NTFS then sets a bit in the file's
/// attributes that the system's own interface reports as something else.
pub(crate) const F_HAS_EA: u8 = 0x40;

/// What the attributes of a record (or of a file's extension records) add up
/// to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Contrib {
    /// Size of the unnamed data stream.
    pub logical: u64,
    /// Bytes of clusters the unnamed data stream occupies.
    pub alloc: u64,
    /// Sizes of the named data streams together.
    pub ads_logical: u64,
    pub ads_alloc: u64,
    /// Bytes of clusters held by the other non-resident attributes (index
    /// buffers, bitmaps, attribute lists, extended attributes).
    pub other_alloc: u64,
    /// How many named data streams.
    pub ads_streams: u32,
    pub flags: u8,
}

impl Contrib {
    pub fn add(&mut self, o: &Contrib) {
        self.logical = self.logical.saturating_add(o.logical);
        self.alloc = self.alloc.saturating_add(o.alloc);
        self.ads_logical = self.ads_logical.saturating_add(o.ads_logical);
        self.ads_alloc = self.ads_alloc.saturating_add(o.ads_alloc);
        self.other_alloc = self.other_alloc.saturating_add(o.other_alloc);
        self.ads_streams = self.ads_streams.saturating_add(o.ads_streams);
        self.flags |= o.flags & (F_DAMAGED | F_COMPRESSED_OR_SPARSE | F_RESIDENT_DATA | F_HAS_EA);
    }
}

/// A file, as the table describes it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FileRec {
    pub c: Contrib,
    pub mtime: i64,
    pub attrs: u32,
    pub reparse_tag: u32,
    /// First name in the chain, or [`NONE`].
    pub name_head: u32,
    pub seq: u16,
    /// Extension records folded into this file.
    pub ext_seen: u16,
    /// Names in the chain.
    pub name_count: u16,
    pub flags: u8,
}

impl FileRec {
    pub fn present(&self) -> bool {
        self.flags & F_PRESENT != 0
    }

    pub fn is_dir(&self) -> bool {
        self.flags & F_DIR != 0
    }

    pub fn damaged(&self) -> bool {
        self.flags & F_DAMAGED != 0 || self.c.flags & F_DAMAGED != 0
    }
}

/// One `$FILE_NAME`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NameRec {
    /// The parent directory as a file reference.
    pub parent: u64,
    /// Next name of the same file, or [`NONE`].
    pub next: u32,
    /// The name as WTF-8 in the pool.
    pub off: u32,
    pub len: u16,
    /// 0 POSIX, 1 Win32, 2 DOS, 3 Win32 and DOS.
    pub ns: u8,
}

pub(crate) const NS_DOS: u8 = 2;

pub(crate) struct BaseOut {
    pub rec: u32,
    pub file: FileRec,
}

pub(crate) struct ExtOut {
    pub rec: u32,
    pub seq: u16,
    pub base: u64,
    pub base_seq: u16,
    pub c: Contrib,
    pub name_head: u32,
    pub name_last: u32,
    /// Names in the chain.
    pub name_count: u16,
}

/// What a file's attribute list says about where its attributes are.
pub(crate) enum ListOut {
    /// Resident list: the extension records it names, sorted, with their
    /// sequence numbers (0 if the list does not give one).
    Refs(u32, Vec<(u32, u16)>),
    /// A list stored in clusters; read later.
    Stored { rec: u32, size: u64, runs: Vec<u8> },
    /// A list that cannot be parsed.
    Broken(u32),
}

/// What one chunk of the table contained.
#[derive(Default)]
pub(crate) struct ChunkOut {
    /// Number of the first record of the chunk.
    pub first: u64,
    pub bases: Vec<BaseOut>,
    pub exts: Vec<ExtOut>,
    pub lists: Vec<ListOut>,
    pub names: Vec<NameRec>,
    pub pool: Vec<u8>,
    pub diag: Diagnostics,
    /// Records known to be lost: unreadable, or in use but unparseable.
    pub lost: u64,
    /// Heap bytes of this output already charged to the memory limit.
    pub charged: u64,
    /// Bytes held by the attribute lists in `lists`.
    pub list_bytes: u64,
}

impl ChunkOut {
    /// The heap this output holds, by capacity: what the limit is charged.
    pub fn bytes(&self) -> u64 {
        fn of<T>(v: &Vec<T>) -> u64 {
            (v.capacity() as u64).saturating_mul(std::mem::size_of::<T>() as u64)
        }
        of(&self.names)
            .saturating_add(of(&self.pool))
            .saturating_add(of(&self.bases))
            .saturating_add(of(&self.exts))
            .saturating_add(of(&self.lists))
            .saturating_add(self.list_bytes)
    }
}

/// Limits on what a name can be. NTFS names are at most 255 UTF-16 units,
/// which take at most 3 bytes each in WTF-8.
const MAX_NAME_UNITS: usize = 255;

/// Most names the reader keeps for one file. NTFS allows 1,024 hard links to
/// a file, and each name may have a short (DOS) alias beside it, so no honest
/// file has more. A file that claims more is damaged: the names past the
/// limit are dropped and the file is marked. One record holds at most about
/// 680 names (a 64 KiB record is the largest), so only a file whose names
/// are spread over extension records can reach the limit; the check is where
/// those records are joined to their file.
pub(crate) const MAX_NAMES_PER_FILE: usize = 2 * 1024;

/// Removes the DOS-only names that have a long name in the same directory:
/// the short alias is another spelling of the same name, not another link.
/// `key` gives a name's directory (its record number) and whether it is a
/// DOS-only name. Sorting the long names' directories once keeps this
/// O(n log n) however many names a file claims.
pub(crate) fn drop_dos_aliases<T>(items: &mut Vec<T>, key: impl Fn(&T) -> (u64, bool)) {
    if items.len() < 2 {
        return;
    }
    let mut long_in: Vec<u64> = items
        .iter()
        .filter_map(|t| {
            let (dir, dos) = key(t);
            (!dos).then_some(dir)
        })
        .collect();
    long_in.sort_unstable();
    long_in.dedup();
    items.retain(|t| {
        let (dir, dos) = key(t);
        !dos || long_in.binary_search(&dir).is_err()
    });
}

pub(crate) struct Parser<'g> {
    geo: &'g Geometry,
    charge: &'g Charge,
    /// Names of the record being read: parent, namespace, place in the record.
    names: Vec<(u64, u8, usize, usize)>,
    /// The UTF-16 bytes of those names, back to back.
    raw: Vec<u8>,
    units: Vec<u16>,
}

impl<'g> Parser<'g> {
    pub fn new(geo: &'g Geometry, charge: &'g Charge) -> Parser<'g> {
        Parser {
            geo,
            charge,
            names: Vec::new(),
            raw: Vec::new(),
            units: Vec::new(),
        }
    }

    /// Reads the records in `bytes`, which hold `bytes.len() / record_bytes`
    /// records starting at record number `first`.
    pub fn chunk(&mut self, bytes: &mut [u8], first: u64, out: &mut ChunkOut) {
        let size = usize::try_from(self.geo.record_bytes).unwrap_or(usize::MAX);
        if size == 0 {
            return;
        }
        for (i, rec) in bytes.chunks_exact_mut(size).enumerate() {
            let no = first.saturating_add(i as u64);
            self.record(rec, no, out);
            // Charge what the record added, and stop as soon as the limit is
            // passed: a hostile record can name far more than it holds, so
            // the check cannot wait for the end of the chunk.
            let now = out.bytes();
            let grew = now.saturating_sub(out.charged);
            out.charged = now;
            if !self.charge.add(grew) {
                return;
            }
        }
    }

    /// Reads one record slot.
    pub fn record(&mut self, rec: &mut [u8], no: u64, out: &mut ChunkOut) {
        match peek(rec) {
            Peek::Unused => return,
            Peek::Bad => {
                out.diag.note(ProblemKind::RecordBadMagic, no, 0);
                out.lost = out.lost.saturating_add(1);
                return;
            }
            Peek::Used => {}
        }
        let header = match fix_and_parse(rec) {
            Ok(h) => h,
            Err(RecordError::Fixup) => {
                out.diag.note(ProblemKind::RecordFixup, no, 0);
                out.lost = out.lost.saturating_add(1);
                return;
            }
            Err(RecordError::Header) => {
                out.diag.note(ProblemKind::RecordHeader, no, 0);
                out.lost = out.lost.saturating_add(1);
                return;
            }
        };
        let Ok(rec_no) = u32::try_from(no) else {
            out.diag.note(ProblemKind::RecordHeader, no, 1);
            out.lost = out.lost.saturating_add(1);
            return;
        };
        // A record that says it is another number is a stale copy or a
        // table with repeated runs: its place says nothing about what it is,
        // so it is not trusted.
        if header.number.is_some_and(|n| n != rec_no) {
            out.diag.note(ProblemKind::RecordHeader, no, 2);
            out.lost = out.lost.saturating_add(1);
            return;
        }
        let rec: &[u8] = rec;
        self.names.clear();
        self.raw.clear();
        let mut file = FileRec {
            seq: header.seq,
            name_head: NONE,
            flags: F_PRESENT | if header.is_dir() { F_DIR } else { 0 },
            ..FileRec::default()
        };
        let mut have_std = false;
        let mut name_flags = 0u32;
        let mut list: Option<ListOut> = None;
        let is_base = header.base.is_none();

        for item in attrs(rec, &header) {
            let attr = match item {
                Ok(a) => a,
                Err(_) => {
                    out.diag.note(ProblemKind::AttributeCorrupt, no, 0);
                    file.c.flags |= F_DAMAGED;
                    break;
                }
            };
            match attr.ty {
                ty::STANDARD_INFORMATION if is_base => {
                    if let Body::Resident(v) = attr.body
                        && let (Some(mtime), Some(attrs)) = (i64_at(v, 8), u32_at(v, 32))
                    {
                        file.mtime = filetime_to_unix(mtime);
                        file.attrs = attrs;
                        have_std = true;
                    }
                }
                ty::FILE_NAME => self.file_name(&attr, no, &mut name_flags, out),
                ty::ATTRIBUTE_LIST => {
                    file.flags |= F_HAS_LIST;
                    if is_base {
                        list = Some(self.attribute_list(&attr, rec_no, no, out));
                    }
                }
                ty::DATA => self.data(&attr, no, &mut file.c, out),
                ty::REPARSE_POINT => {
                    if let Body::Resident(v) = attr.body
                        && let Some(tag) = u32_at(v, 0)
                    {
                        file.reparse_tag = tag;
                    }
                }
                0xD0 | 0xE0 => {
                    file.c.flags |= F_HAS_EA;
                    self.other(&attr, no, &mut file.c, out);
                }
                _ => self.other(&attr, no, &mut file.c, out),
            }
        }
        if !have_std {
            // Without the standard information, the flags the names carry are
            // the next best account of what the file is.
            file.attrs = name_flags;
        }

        // Names go to the pool, DOS-only aliases of a name in the same
        // directory left out.
        drop_dos_aliases(&mut self.names, |&(p, ns, _, _)| {
            (split_ref(p).0, ns == NS_DOS)
        });
        let first_name = out.names.len();
        let mut kept = 0usize;
        for k in 0..self.names.len() {
            let Some(&(parent, ns, at, len)) = self.names.get(k) else {
                continue;
            };
            let units = &mut self.units;
            units.clear();
            if let Some(raw) = bytes_at(&self.raw, at, len) {
                units.extend(
                    raw.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c)),
                );
            }
            let off = out.pool.len();
            wtf8_from_utf16(units.iter().copied(), &mut out.pool);
            let wrote = out.pool.len().saturating_sub(off);
            let (Ok(off32), Ok(len16)) = (u32::try_from(off), u16::try_from(wrote)) else {
                out.pool.truncate(off);
                out.diag.note(ProblemKind::NameCorrupt, no, 2);
                continue;
            };
            let idx = out.names.len();
            let next = match u32::try_from(idx.saturating_add(1)) {
                Ok(n) => n,
                Err(_) => NONE,
            };
            out.names.push(NameRec {
                parent,
                next,
                off: off32,
                len: len16,
                ns,
            });
            kept = kept.saturating_add(1);
        }
        let (head, last) = if kept == 0 {
            (NONE, NONE)
        } else {
            let head = u32::try_from(first_name).unwrap_or(NONE);
            let last_at = out.names.len().saturating_sub(1);
            let last = u32::try_from(last_at).unwrap_or(NONE);
            if let Some(n) = out.names.get_mut(last_at) {
                n.next = NONE;
            }
            (head, last)
        };

        match header.base {
            None => {
                file.name_head = head;
                file.name_count = u16::try_from(kept).unwrap_or(u16::MAX);
                if let Some(l) = list {
                    out.list_bytes = out.list_bytes.saturating_add(list_cost(&l));
                    out.lists.push(l);
                }
                out.bases.push(BaseOut { rec: rec_no, file });
            }
            Some((base, base_seq)) => {
                out.exts.push(ExtOut {
                    rec: rec_no,
                    seq: header.seq,
                    base,
                    base_seq,
                    c: file.c,
                    name_head: head,
                    name_last: last,
                    name_count: u16::try_from(kept).unwrap_or(u16::MAX),
                });
            }
        }
    }

    fn file_name(&mut self, attr: &Attr<'_>, no: u64, name_flags: &mut u32, out: &mut ChunkOut) {
        let Body::Resident(v) = attr.body else {
            out.diag.note(ProblemKind::NameCorrupt, no, 0);
            return;
        };
        let (Some(parent), Some(flags), Some(n), Some(ns)) = (
            u64_at(v, 0),
            u32_at(v, 0x38),
            u8_at(v, 0x40),
            u8_at(v, 0x41),
        ) else {
            out.diag.note(ProblemKind::NameCorrupt, no, 1);
            return;
        };
        let units = usize::from(n);
        if units == 0 || units > MAX_NAME_UNITS || ns > 3 {
            out.diag.note(ProblemKind::NameCorrupt, no, 2);
            return;
        }
        let Some(raw) = bytes_at(v, 0x42, units.saturating_mul(2)) else {
            out.diag.note(ProblemKind::NameCorrupt, no, 3);
            return;
        };
        let at = self.raw.len();
        self.raw.extend_from_slice(raw);
        *name_flags |= flags;
        self.names.push((parent, ns, at, raw.len()));
    }

    fn attribute_list(
        &mut self,
        attr: &Attr<'_>,
        rec_no: u32,
        no: u64,
        out: &mut ChunkOut,
    ) -> ListOut {
        match attr.body {
            Body::Resident(v) => {
                let mut refs: Vec<(u32, u16)> = Vec::new();
                match list_entries(v, |e| {
                    let (r, s) = split_ref(e.reference);
                    if r != u64::from(rec_no) {
                        match u32::try_from(r) {
                            Ok(r) => refs.push((r, s)),
                            // A record number the table cannot hold: keep it
                            // as a reference that will not resolve.
                            Err(_) => refs.push((u32::MAX, s)),
                        }
                    }
                }) {
                    Ok(()) => {
                        refs.sort_unstable();
                        refs.dedup_by_key(|r| r.0);
                        ListOut::Refs(rec_no, refs)
                    }
                    Err(()) => {
                        out.diag.note(ProblemKind::AttributeListCorrupt, no, 0);
                        ListOut::Broken(rec_no)
                    }
                }
            }
            Body::NonResident(n) => ListOut::Stored {
                rec: rec_no,
                size: n.size,
                runs: n.runs.to_vec(),
            },
        }
    }

    /// A `$DATA` stream: the unnamed one is the file's content, named ones
    /// are alternate streams.
    fn data(&mut self, attr: &Attr<'_>, no: u64, c: &mut Contrib, out: &mut ChunkOut) {
        let named = !attr.is_unnamed();
        let (logical, alloc) = match attr.body {
            Body::Resident(v) => (v.len() as u64, 0),
            Body::NonResident(n) => {
                let Some(clusters) = self.clusters_of(&n, no, c, out) else {
                    return;
                };
                let alloc = clusters.saturating_mul(u64::from(self.geo.cluster_bytes));
                // Only the first piece of a stream says how long it is.
                let logical = if n.lowest_vcn == 0 { n.size } else { 0 };
                (logical, alloc)
            }
        };
        let head = match attr.body {
            Body::Resident(_) => true,
            Body::NonResident(n) => n.lowest_vcn == 0,
        };
        if named {
            c.ads_logical = c.ads_logical.saturating_add(logical);
            c.ads_alloc = c.ads_alloc.saturating_add(alloc);
            if head {
                c.ads_streams = c.ads_streams.saturating_add(1);
            }
        } else {
            c.logical = c.logical.saturating_add(logical);
            c.alloc = c.alloc.saturating_add(alloc);
            if matches!(attr.body, Body::Resident(_)) {
                c.flags |= F_RESIDENT_DATA;
            }
            if attr.flags & (ATTR_COMPRESSED | ATTR_SPARSE) != 0 {
                c.flags |= F_COMPRESSED_OR_SPARSE;
            }
        }
    }

    /// Any other attribute: only the clusters it holds matter.
    fn other(&mut self, attr: &Attr<'_>, no: u64, c: &mut Contrib, out: &mut ChunkOut) {
        if let Body::NonResident(n) = attr.body
            && let Some(clusters) = self.clusters_of(&n, no, c, out)
        {
            c.other_alloc = c
                .other_alloc
                .saturating_add(clusters.saturating_mul(u64::from(self.geo.cluster_bytes)));
        }
    }

    /// Clusters a non-resident attribute occupies, from its run list. A list
    /// that does not decode marks the file damaged.
    fn clusters_of(
        &mut self,
        n: &crate::record::NonResident<'_>,
        no: u64,
        c: &mut Contrib,
        out: &mut ChunkOut,
    ) -> Option<u64> {
        match runlist::decode(n.runs, n.lowest_vcn, self.geo.clusters, |_| {}) {
            Ok(t) => {
                if n.lowest_vcn == 0 && n.unit == 0 && t.holes == 0 {
                    // An ordinary stream is fully allocated, and the first
                    // piece of a stream that continues in an extension
                    // record says how long the whole is, so the runs of one
                    // piece may hold less than it says but never more. The
                    // runs win either way.
                    let said = n.allocated;
                    let have = t
                        .allocated
                        .saturating_mul(u64::from(self.geo.cluster_bytes));
                    if have > said {
                        out.diag.note(ProblemKind::SizeMismatch, no, said);
                    }
                }
                Some(t.allocated)
            }
            Err(_) => {
                out.diag.note(ProblemKind::RunlistCorrupt, no, 0);
                c.flags |= F_DAMAGED;
                None
            }
        }
    }
}

/// Heap bytes an attribute list output holds beyond its own slot.
fn list_cost(l: &ListOut) -> u64 {
    let bytes = match l {
        ListOut::Refs(_, refs) => refs
            .capacity()
            .saturating_mul(std::mem::size_of::<(u32, u16)>()),
        ListOut::Stored { runs, .. } => runs.capacity(),
        ListOut::Broken(_) => 0,
    };
    bytes as u64
}

/// One entry of an attribute list.
pub(crate) struct ListEntry {
    pub ty: u32,
    pub lowest_vcn: u64,
    pub reference: u64,
    pub named: bool,
}

/// Walks an attribute list's entries. `Err` when an entry does not fit.
pub(crate) fn list_entries(value: &[u8], mut each: impl FnMut(ListEntry)) -> Result<(), ()> {
    let mut pos = 0usize;
    while pos < value.len() {
        let rest = value.get(pos..).ok_or(())?;
        let ty = u32_at(rest, 0).ok_or(())?;
        let len = usize::from(u16_at(rest, 4).ok_or(())?);
        if ty == 0 {
            return Ok(());
        }
        if len < 0x1A || len > rest.len() {
            return Err(());
        }
        each(ListEntry {
            ty,
            named: u8_at(rest, 6).ok_or(())? != 0,
            lowest_vcn: u64_at(rest, 8).ok_or(())?,
            reference: u64_at(rest, 16).ok_or(())?,
        });
        // Entries are padded to eight bytes; a length that is not is damage.
        if len % 8 != 0 {
            return Err(());
        }
        pos = pos.checked_add(len).ok_or(())?;
    }
    Ok(())
}
