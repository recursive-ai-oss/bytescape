//! The batch format: plain data a scanner produces and the tree applies.
//!
//! A [`Batch`] is a list of [`Record`]s plus one block of name bytes the
//! records point into. It holds no pointers, so it can be sent between
//! threads as is, or serialized with [`Batch::encode`] / [`Batch::decode`]
//! for another process. The serialized form starts with a version number.
//!
//! # What a scanner sends
//!
//! * One [`Record::Entry`] per directory entry it finds. The first record of a
//!   scan is the root directory, with parent [`DirKey::NONE`].
//! * A directory entry carries a [`DirKey`] chosen by the scanner (any `u32`
//!   except [`DirKey::NONE`]; a shared counter works). **Keys must be unique
//!   within a scan**; this is the scanner's side of the contract. Children
//!   name their parent by the key, so if two directories shared one the tree
//!   could not tell which a child belongs to. A directory record whose key is
//!   already in use (applied, or waiting for its parent) is refused with
//!   [`ApplyError::DuplicateKey`](crate::ApplyError::DuplicateKey) and the
//!   directory it would have been listed under is flagged as having lost
//!   records. The tree stays correct and fast, but it does not guess which of
//!   the two was meant.
//! * When a directory has been listed completely, [`Record::ListingDone`].
//!   When it could not be listed (or the listing failed midway),
//!   [`Record::Unreadable`] instead; it counts as finished listing.
//!
//! Records of one directory must reach the tree in the order they were
//! produced. Records of different directories may arrive in any order: a
//! record whose parent directory has not arrived yet is held back until it
//! does.

use crate::error::{BatchFull, DecodeError};
use crate::types::{Category, DirKey, FileKey, NodeFlags, NodeKind, ReadErrorKind, ReadFailure};

/// The serialized format version written by [`Batch::encode`]. Version 2
/// added the optional exclusive-bytes field of shared entries; a decoder of
/// version 1 would refuse such an entry, so the number moved with it.
pub const BATCH_FORMAT_VERSION: u16 = 2;

const MAGIC: [u8; 4] = *b"BSTB";
const HEADER_LEN: usize = 16;
/// The smallest possible serialized record (`ListingDone`).
const MIN_RECORD_LEN: usize = 5;

/// The most memory [`Batch::decode`] uses for the records, as a multiple of
/// the input size (excluding the names block, which is copied once). It is
/// the in-memory record size divided by the smallest serialized record.
pub const DECODE_MAX_EXPANSION: usize = std::mem::size_of::<Record>().div_ceil(MIN_RECORD_LEN);

const TAG_ENTRY: u8 = 1;
const TAG_LISTING_DONE: u8 = 2;
const TAG_UNREADABLE: u8 = 3;

const OPT_LINK: u8 = 1;
const OPT_FAILURE: u8 = 2;
const OPT_EXCLUSIVE: u8 = 4;

/// Where an entry's name lies in the batch's name block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NameSpan {
    /// Start, in bytes.
    pub offset: u32,
    /// Length, in bytes.
    pub len: u32,
}

/// What a scanner knows about one entry besides its name and place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Meta {
    /// Logical (apparent) size in bytes. For a directory: its own entry only.
    pub logical: u64,
    /// Allocated size in bytes. For a directory: its own entry only.
    pub allocated: u64,
    /// Modification time in whole seconds since the Unix epoch. Finer
    /// precision is not kept by the tree.
    pub mtime: i64,
    /// Scanner-set flags; flags in [`NodeFlags::PRODUCER_MASK`] only, others
    /// are ignored. A scanner that knows a file has other names (link count
    /// above one) may set [`NodeFlags::HARD_LINK`] itself.
    pub flags: NodeFlags,
    /// Category, if the scanner knows it.
    pub category: Category,
    /// Identity for hard-link detection. A scanner that cannot tell which
    /// files have several names (no link count) gives one to every file that
    /// has an identity; one that can tell gives it to files with a link count
    /// above one. The tree counts the first name seen and treats later names
    /// with the same key as links to it, flagging both; a key seen once flags
    /// nothing. Entries without a key are never treated as hard links.
    /// Ignored for directories.
    pub link: Option<FileKey>,
    /// Set when the entry itself could not be inspected. For a directory this
    /// means it could not be opened, and it counts as finished listing.
    pub failure: Option<ReadFailure>,
    /// For an entry flagged [`NodeFlags::SHARED`]: how many of its
    /// `allocated` bytes belong to this entry alone, so deleting it frees
    /// them. The rest is also used by other files (reflinks, snapshots) or
    /// counted at another name. Ignored, and read as zero, for an entry that
    /// is not shared; kept at most `allocated`.
    pub exclusive: u64,
}

/// A directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// What it is.
    pub kind: NodeKind,
    /// For directories, this directory's key, which must be unique within
    /// the scan (see the module documentation). Unused for other kinds.
    pub key: DirKey,
    /// The containing directory's key, or [`DirKey::NONE`] for the root.
    pub parent: DirKey,
    /// The name, in the batch's name block.
    pub name: NameSpan,
    /// Sizes, time, flags and the rest.
    pub meta: Meta,
}

/// One unit of scanner output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// A directory entry was found.
    Entry(Entry),
    /// The directory was listed completely.
    ListingDone {
        /// The directory.
        dir: DirKey,
    },
    /// The directory could not be listed (completely).
    Unreadable {
        /// The directory.
        dir: DirKey,
        /// Why.
        failure: ReadFailure,
    },
}

/// A group of records and the names they point at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Batch {
    records: Vec<Record>,
    names: Vec<u8>,
}

impl Batch {
    /// An empty batch.
    pub fn new() -> Batch {
        Batch::default()
    }

    /// Removes everything, keeping the allocations for reuse.
    pub fn clear(&mut self) {
        self.records.clear();
        self.names.clear();
    }

    /// Number of records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether there are no records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The records, in order.
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Size of the name block in bytes, for deciding when to flush.
    pub fn name_bytes(&self) -> usize {
        self.names.len()
    }

    /// The bytes of a name, or `None` when the span lies outside the block.
    pub fn name(&self, span: NameSpan) -> Option<&[u8]> {
        let start = usize::try_from(span.offset).ok()?;
        let end = start.checked_add(usize::try_from(span.len).ok()?)?;
        self.names.get(start..end)
    }

    fn add_name(&mut self, name: &[u8]) -> Result<NameSpan, BatchFull> {
        let offset = u32::try_from(self.names.len()).map_err(|_| BatchFull)?;
        let len = u32::try_from(name.len()).map_err(|_| BatchFull)?;
        offset.checked_add(len).ok_or(BatchFull)?;
        self.names.extend_from_slice(name);
        Ok(NameSpan { offset, len })
    }

    /// Adds a directory. Use [`DirKey::NONE`] as `parent` for the scan root.
    pub fn dir(
        &mut self,
        key: DirKey,
        parent: DirKey,
        name: &[u8],
        meta: &Meta,
    ) -> Result<(), BatchFull> {
        self.entry(NodeKind::Dir, key, parent, name, meta)
    }

    /// Adds a non-directory entry (`File`, `Symlink` or `Other`) to the
    /// directory `parent`.
    pub fn file(
        &mut self,
        kind: NodeKind,
        parent: DirKey,
        name: &[u8],
        meta: &Meta,
    ) -> Result<(), BatchFull> {
        self.entry(kind, DirKey(0), parent, name, meta)
    }

    fn entry(
        &mut self,
        kind: NodeKind,
        key: DirKey,
        parent: DirKey,
        name: &[u8],
        meta: &Meta,
    ) -> Result<(), BatchFull> {
        let span = self.add_name(name)?;
        self.records.push(Record::Entry(Entry {
            kind,
            key,
            parent,
            name: span,
            meta: *meta,
        }));
        Ok(())
    }

    /// Adds a record as is, without checking it against the names block.
    #[cfg(test)]
    pub(crate) fn push_raw(&mut self, record: Record) {
        self.records.push(record);
    }

    /// Reports that `dir` has been listed completely.
    pub fn listing_done(&mut self, dir: DirKey) {
        self.records.push(Record::ListingDone { dir });
    }

    /// Reports that `dir` could not be listed (completely). This counts as
    /// finishing its listing, and entries sent before it are kept. A
    /// [`Batch::listing_done`] for the same directory afterwards is accepted
    /// and changes nothing, so a scanner that sends both does not produce
    /// errors.
    pub fn unreadable(&mut self, dir: DirKey, failure: ReadFailure) {
        self.records.push(Record::Unreadable { dir, failure });
    }

    /// Appends the serialized form to `out`. All integers are little-endian.
    ///
    /// Returns `Err` only if the batch is too large to describe (more than
    /// `u32::MAX` records or name bytes), in which case `out` is unchanged.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), BatchFull> {
        let count = u32::try_from(self.records.len()).map_err(|_| BatchFull)?;
        let names_len = u32::try_from(self.names.len()).map_err(|_| BatchFull)?;
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&BATCH_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&names_len.to_le_bytes());
        out.extend_from_slice(&self.names);
        for record in &self.records {
            match record {
                Record::Entry(entry) => {
                    let meta = &entry.meta;
                    let mut opt = 0u8;
                    if meta.link.is_some() {
                        opt |= OPT_LINK;
                    }
                    if meta.failure.is_some() {
                        opt |= OPT_FAILURE;
                    }
                    if meta.flags.contains(NodeFlags::SHARED) {
                        opt |= OPT_EXCLUSIVE;
                    }
                    out.push(TAG_ENTRY);
                    out.push(entry.kind as u8);
                    out.extend_from_slice(&meta.flags.bits().to_le_bytes());
                    out.push(meta.category.0);
                    out.push(opt);
                    out.extend_from_slice(&entry.key.0.to_le_bytes());
                    out.extend_from_slice(&entry.parent.0.to_le_bytes());
                    out.extend_from_slice(&entry.name.offset.to_le_bytes());
                    out.extend_from_slice(&entry.name.len.to_le_bytes());
                    out.extend_from_slice(&meta.logical.to_le_bytes());
                    out.extend_from_slice(&meta.allocated.to_le_bytes());
                    out.extend_from_slice(&meta.mtime.to_le_bytes());
                    if let Some(link) = meta.link {
                        out.extend_from_slice(&link.volume.to_le_bytes());
                        out.extend_from_slice(&link.id.to_le_bytes());
                    }
                    if let Some(failure) = meta.failure {
                        out.push(failure.kind as u8);
                        out.extend_from_slice(&failure.code.to_le_bytes());
                    }
                    if opt & OPT_EXCLUSIVE != 0 {
                        out.extend_from_slice(&meta.exclusive.to_le_bytes());
                    }
                }
                Record::ListingDone { dir } => {
                    out.push(TAG_LISTING_DONE);
                    out.extend_from_slice(&dir.0.to_le_bytes());
                }
                Record::Unreadable { dir, failure } => {
                    out.push(TAG_UNREADABLE);
                    out.extend_from_slice(&dir.0.to_le_bytes());
                    out.push(failure.kind as u8);
                    out.extend_from_slice(&failure.code.to_le_bytes());
                }
            }
        }
        Ok(())
    }

    /// Reads a serialized batch.
    ///
    /// Nothing is reserved on the strength of a header field alone: the
    /// record count is first proven by walking the records (every length
    /// checked against the data), and only then is memory reserved, exactly,
    /// for the records that are really there.
    ///
    /// **Memory use.** The batch takes at most [`DECODE_MAX_EXPANSION`] times
    /// the input size, plus one copy of the names block. The worst case is an
    /// input made only of 5-byte end-of-listing records, each of which takes
    /// a full in-memory record; ordinary entries (46 bytes or more each) cost
    /// about three times their size. A transport that caps its frame size
    /// should budget with that factor.
    pub fn decode(bytes: &[u8]) -> Result<Batch, DecodeError> {
        let mut cur = Cursor { bytes, pos: 0 };
        if cur.take(4)? != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let version = cur.u16()?;
        if version != BATCH_FORMAT_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        if cur.u16()? != 0 {
            return Err(DecodeError::ReservedBits);
        }
        let count = to_usize(cur.u32()?)?;
        let names_len = to_usize(cur.u32()?)?;
        debug_assert_eq!(cur.pos, HEADER_LEN);
        let names = cur
            .take(names_len)
            .map_err(|_| DecodeError::BadLength)?
            .to_vec();
        if count > cur.remaining() / MIN_RECORD_LEN {
            return Err(DecodeError::BadLength);
        }
        // First pass: walk the records without building anything, so the
        // allocation below is sized by data that exists.
        let mut probe = Cursor {
            bytes: cur.bytes,
            pos: cur.pos,
        };
        for _ in 0..count {
            skip_record(&mut probe)?;
        }
        if probe.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        let mut records = Vec::with_capacity(count);
        for _ in 0..count {
            records.push(decode_record(&mut cur, names.len())?);
        }
        if cur.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(Batch { records, names })
    }
}

/// Moves past one serialized record, checking only its shape.
fn skip_record(cur: &mut Cursor<'_>) -> Result<(), DecodeError> {
    let len = match cur.u8()? {
        TAG_ENTRY => {
            // After the tag: kind, flags (2), category, options, key, parent,
            // name offset and length, logical, allocated, mtime.
            const FIXED: usize = 1 + 2 + 1 + 1 + 4 + 4 + 4 + 4 + 8 + 8 + 8;
            let head = cur.take(FIXED)?;
            let opt = head.get(4).copied().ok_or(DecodeError::Truncated)?;
            if opt & !(OPT_LINK | OPT_FAILURE | OPT_EXCLUSIVE) != 0 {
                return Err(DecodeError::BadValue);
            }
            (if opt & OPT_LINK != 0 { 8 + 16 } else { 0 })
                + (if opt & OPT_FAILURE != 0 { 1 + 4 } else { 0 })
                + (if opt & OPT_EXCLUSIVE != 0 { 8 } else { 0 })
        }
        TAG_LISTING_DONE => 4,
        TAG_UNREADABLE => 4 + 1 + 4,
        _ => return Err(DecodeError::BadValue),
    };
    cur.take(len)?;
    Ok(())
}

fn to_usize(value: u32) -> Result<usize, DecodeError> {
    usize::try_from(value).map_err(|_| DecodeError::BadLength)
}

fn decode_failure(cur: &mut Cursor<'_>) -> Result<ReadFailure, DecodeError> {
    let kind = ReadErrorKind::from_u8(cur.u8()?).ok_or(DecodeError::BadValue)?;
    let code = cur.i32()?;
    Ok(ReadFailure { kind, code })
}

fn decode_record(cur: &mut Cursor<'_>, names_len: usize) -> Result<Record, DecodeError> {
    match cur.u8()? {
        TAG_ENTRY => {
            let kind = NodeKind::from_u8(cur.u8()?).ok_or(DecodeError::BadValue)?;
            let flags = NodeFlags::from_bits_truncate(cur.u16()?).and(NodeFlags::PRODUCER_MASK);
            let category = Category(cur.u8()?);
            let opt = cur.u8()?;
            if opt & !(OPT_LINK | OPT_FAILURE | OPT_EXCLUSIVE) != 0 {
                return Err(DecodeError::BadValue);
            }
            let key = DirKey(cur.u32()?);
            let parent = DirKey(cur.u32()?);
            let name = NameSpan {
                offset: cur.u32()?,
                len: cur.u32()?,
            };
            let end = to_usize(name.offset)?
                .checked_add(to_usize(name.len)?)
                .ok_or(DecodeError::BadNameSpan)?;
            if end > names_len {
                return Err(DecodeError::BadNameSpan);
            }
            let logical = cur.u64()?;
            let allocated = cur.u64()?;
            let mtime = cur.i64()?;
            let link = if opt & OPT_LINK != 0 {
                Some(FileKey {
                    volume: cur.u64()?,
                    id: cur.u128()?,
                })
            } else {
                None
            };
            let failure = if opt & OPT_FAILURE != 0 {
                Some(decode_failure(cur)?)
            } else {
                None
            };
            let exclusive = if opt & OPT_EXCLUSIVE != 0 {
                cur.u64()?
            } else {
                0
            };
            Ok(Record::Entry(Entry {
                kind,
                key,
                parent,
                name,
                meta: Meta {
                    logical,
                    allocated,
                    mtime,
                    flags,
                    category,
                    link,
                    failure,
                    exclusive: if flags.contains(NodeFlags::SHARED) {
                        exclusive
                    } else {
                        0
                    },
                },
            }))
        }
        TAG_LISTING_DONE => Ok(Record::ListingDone {
            dir: DirKey(cur.u32()?),
        }),
        TAG_UNREADABLE => {
            let dir = DirKey(cur.u32()?);
            let failure = decode_failure(cur)?;
            Ok(Record::Unreadable { dir, failure })
        }
        _ => Err(DecodeError::BadValue),
    }
}

/// Bounds-checked reader over a byte slice.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::Truncated)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(DecodeError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        self.take(N)?.try_into().map_err(|_| DecodeError::Truncated)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn u128(&mut self) -> Result<u128, DecodeError> {
        Ok(u128::from_le_bytes(self.array()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Batch {
        let mut b = Batch::new();
        let meta = Meta {
            logical: 10,
            allocated: 4096,
            mtime: -5,
            flags: NodeFlags::SHARED | NodeFlags::COMPLETE,
            category: Category(3),
            link: Some(FileKey {
                volume: 7,
                id: 1 << 100,
            }),
            failure: Some(ReadFailure {
                kind: ReadErrorKind::Io,
                code: -5,
            }),
            exclusive: 1000,
        };
        b.dir(DirKey(0), DirKey::NONE, b"/root", &Meta::default())
            .unwrap();
        b.file(NodeKind::File, DirKey(0), b"a\xff\n", &meta)
            .unwrap();
        b.listing_done(DirKey(0));
        b.unreadable(
            DirKey(9),
            ReadFailure {
                kind: ReadErrorKind::PermissionDenied,
                code: 13,
            },
        );
        b
    }

    #[test]
    fn round_trip() {
        let batch = sample();
        let mut bytes = Vec::new();
        batch.encode(&mut bytes).unwrap();
        let back = Batch::decode(&bytes).unwrap();
        // COMPLETE is not a producer flag and does not survive.
        let Record::Entry(e) = back.records()[1] else {
            panic!("entry expected");
        };
        assert_eq!(e.meta.flags, NodeFlags::SHARED);
        assert_eq!(e.meta.exclusive, 1000);
        assert_eq!(back.name(e.name), Some(&b"a\xff\n"[..]));
        assert_eq!(back.len(), batch.len());
        assert_eq!(back.records()[2], batch.records()[2]);
        assert_eq!(back.records()[3], batch.records()[3]);
        let mut again = Vec::new();
        back.encode(&mut again).unwrap();
        let back2 = Batch::decode(&again).unwrap();
        assert_eq!(back, back2);
    }

    #[test]
    fn decode_rejects_every_truncation() {
        let mut bytes = Vec::new();
        sample().encode(&mut bytes).unwrap();
        for cut in 0..bytes.len() {
            assert!(Batch::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        bytes.push(0);
        assert_eq!(Batch::decode(&bytes), Err(DecodeError::TrailingBytes));
    }

    #[test]
    fn decode_rejects_bad_header_and_values() {
        let mut bytes = Vec::new();
        sample().encode(&mut bytes).unwrap();
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert_eq!(Batch::decode(&bad), Err(DecodeError::BadMagic));
        // The header carries the version the encoder wrote, so a stream from
        // an older format is refused by number, not by luck.
        assert_eq!(bytes[4..6], BATCH_FORMAT_VERSION.to_le_bytes());
        assert_eq!(BATCH_FORMAT_VERSION, 2);
        let mut older = bytes.clone();
        older[4..6].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(
            Batch::decode(&older),
            Err(DecodeError::UnsupportedVersion(1))
        );
        let mut bad = bytes.clone();
        bad[4] = 99;
        assert_eq!(
            Batch::decode(&bad),
            Err(DecodeError::UnsupportedVersion(99))
        );
        let mut bad = bytes.clone();
        bad[6] = 1;
        assert_eq!(Batch::decode(&bad), Err(DecodeError::ReservedBits));
        // A huge record count must fail before allocating.
        let mut bad = bytes.clone();
        bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Batch::decode(&bad), Err(DecodeError::BadLength));
        // A names length past the end.
        let mut bad = bytes;
        bad[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Batch::decode(&bad), Err(DecodeError::BadLength));
    }

    #[test]
    fn decode_rejects_name_span_outside_names() {
        let mut b = Batch::new();
        b.dir(DirKey(0), DirKey::NONE, b"r", &Meta::default())
            .unwrap();
        let mut bytes = Vec::new();
        b.encode(&mut bytes).unwrap();
        // Entry layout after header (16) + names (1): tag, kind, flags(2),
        // category, opt, key(4), parent(4), name_off(4), name_len(4).
        let len_at = HEADER_LEN + 1 + 1 + 1 + 2 + 1 + 1 + 4 + 4 + 4;
        bytes[len_at..len_at + 4].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(Batch::decode(&bytes), Err(DecodeError::BadNameSpan));
    }

    #[test]
    fn decode_reserves_exactly_what_is_there() {
        // A flood of the smallest records: the most memory per input byte.
        let mut b = Batch::new();
        for i in 0..10_000 {
            b.listing_done(DirKey(i));
        }
        let mut bytes = Vec::new();
        b.encode(&mut bytes).unwrap();
        let back = Batch::decode(&bytes).unwrap();
        assert_eq!(back.records.len(), 10_000);
        assert_eq!(back.records.capacity(), back.records.len());
        let in_memory = back.records.capacity() * std::mem::size_of::<Record>();
        assert!(in_memory <= DECODE_MAX_EXPANSION * bytes.len());
        // The documented factor must not creep up unnoticed.
        const { assert!(DECODE_MAX_EXPANSION <= 26) };
    }

    #[test]
    fn a_record_count_larger_than_the_data_fails_before_any_allocation_is_sized_by_it() {
        let mut b = Batch::new();
        b.listing_done(DirKey(1));
        b.listing_done(DirKey(2));
        let mut bytes = Vec::new();
        b.encode(&mut bytes).unwrap();
        // Claim far more records than there are bytes for, but few enough to
        // pass the cheap length check: the walk over the real data catches it.
        let claimed: u32 = 2 + 1;
        bytes[8..12].copy_from_slice(&claimed.to_le_bytes());
        // Pad so that the cheap check (count <= remaining / 5) passes.
        bytes.extend_from_slice(&[0u8; 8]);
        assert!(Batch::decode(&bytes).is_err());
    }

    #[test]
    fn name_lookup_is_checked() {
        let mut b = Batch::new();
        b.file(NodeKind::File, DirKey(0), b"abc", &Meta::default())
            .unwrap();
        assert_eq!(b.name(NameSpan { offset: 1, len: 2 }), Some(&b"bc"[..]));
        assert_eq!(b.name(NameSpan { offset: 1, len: 3 }), None);
        assert_eq!(
            b.name(NameSpan {
                offset: u32::MAX,
                len: u32::MAX
            }),
            None
        );
    }
}
