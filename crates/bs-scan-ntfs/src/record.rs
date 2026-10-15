//! File records: the update sequence, the header, and the attributes in a
//! record's body. Everything is read through checked accessors; a record
//! that does not hold together is refused with the reason.

use crate::le::{bytes_at, u8_at, u16_at, u32_at, u64_at};

/// `FILE_RECORD_SEGMENT_IN_USE`.
pub(crate) const FLAG_IN_USE: u16 = 0x0001;
/// `FILE_FILE_NAME_INDEX_PRESENT`: the record is a directory.
pub(crate) const FLAG_DIRECTORY: u16 = 0x0002;

/// Attribute types.
pub(crate) mod ty {
    pub const STANDARD_INFORMATION: u32 = 0x10;
    pub const ATTRIBUTE_LIST: u32 = 0x20;
    pub const FILE_NAME: u32 = 0x30;
    pub const DATA: u32 = 0x80;
    pub const REPARSE_POINT: u32 = 0xC0;
    pub const END: u32 = 0xFFFF_FFFF;
}

/// Attribute flags.
pub(crate) const ATTR_COMPRESSED: u16 = 0x0001;
pub(crate) const ATTR_SPARSE: u16 = 0x8000;

/// The bytes a sector contributes to the update sequence check.
const SECTOR: usize = 512;

/// What a first look at a record slot says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Peek {
    /// Never used (all zero in the magic) or freed: nothing to read.
    Unused,
    /// In use, with the right magic.
    Used,
    /// Not a record: wrong or interrupted magic.
    Bad,
}

pub(crate) fn peek(rec: &[u8]) -> Peek {
    let Some(magic) = bytes_at(rec, 0, 4) else {
        return Peek::Bad;
    };
    match magic {
        b"FILE" => match u16_at(rec, 0x16) {
            Some(flags) if flags & FLAG_IN_USE != 0 => Peek::Used,
            Some(_) => Peek::Unused,
            None => Peek::Bad,
        },
        [0, 0, 0, 0] => Peek::Unused,
        _ => Peek::Bad,
    }
}

/// Why a record in use cannot be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordError {
    /// The update sequence is invalid or a sector does not match it.
    Fixup,
    /// The header's offsets and lengths are inconsistent.
    Header,
}

/// The parts of a record header the reader uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub seq: u16,
    pub flags: u16,
    /// Where the first attribute starts.
    pub attrs_at: usize,
    /// One past the last byte in use.
    pub used: usize,
    /// The file this record extends, as a record number and sequence number,
    /// or `None` for a base record.
    pub base: Option<(u64, u16)>,
    /// The record's own number, which headers of NTFS 3.1 and later carry
    /// (the update sequence then starts at 0x30). `None` for the older
    /// header, which has no such field.
    pub number: Option<u32>,
}

impl Header {
    pub fn is_dir(&self) -> bool {
        self.flags & FLAG_DIRECTORY != 0
    }
}

/// Splits a 64-bit file reference into record number and sequence number.
pub(crate) fn split_ref(reference: u64) -> (u64, u16) {
    (
        reference & 0x0000_FFFF_FFFF_FFFF,
        u16::try_from(reference >> 48).unwrap_or(0),
    )
}

/// Checks the update sequence and restores the last two bytes of every sector
/// to what they were before the sequence number was stamped over them, then
/// checks the header. The record is changed in place.
pub(crate) fn fix_and_parse(rec: &mut [u8]) -> Result<Header, RecordError> {
    let len = rec.len();
    let usa_at = usize::from(u16_at(rec, 4).ok_or(RecordError::Header)?);
    let usa_count = usize::from(u16_at(rec, 6).ok_or(RecordError::Header)?);
    let sectors = len / SECTOR;
    if !len.is_multiple_of(SECTOR) || sectors == 0 || usa_count != sectors.saturating_add(1) {
        return Err(RecordError::Fixup);
    }
    let usa_end = usa_count
        .checked_mul(2)
        .and_then(|n| n.checked_add(usa_at))
        .ok_or(RecordError::Fixup)?;
    // The array lies inside the first sector, short of the bytes it patches.
    if usa_at < 0x2A || usa_at % 2 != 0 || usa_end > SECTOR - 2 {
        return Err(RecordError::Fixup);
    }
    let usn = u16_at(rec, usa_at).ok_or(RecordError::Fixup)?;
    for i in 1..usa_count {
        let tail = i
            .checked_mul(SECTOR)
            .and_then(|n| n.checked_sub(2))
            .ok_or(RecordError::Fixup)?;
        let saved_at = usa_at
            .checked_add(i.checked_mul(2).ok_or(RecordError::Fixup)?)
            .ok_or(RecordError::Fixup)?;
        let saved: [u8; 2] = bytes_at(rec, saved_at, 2)
            .and_then(|b| b.try_into().ok())
            .ok_or(RecordError::Fixup)?;
        let slot = rec
            .get_mut(tail..tail.saturating_add(2))
            .ok_or(RecordError::Fixup)?;
        if slot != usn.to_le_bytes() {
            return Err(RecordError::Fixup);
        }
        slot.copy_from_slice(&saved);
    }

    let attrs_at = usize::from(u16_at(rec, 0x14).ok_or(RecordError::Header)?);
    let flags = u16_at(rec, 0x16).ok_or(RecordError::Header)?;
    let used = usize::try_from(u32_at(rec, 0x18).ok_or(RecordError::Header)?)
        .map_err(|_| RecordError::Header)?;
    let allocated = usize::try_from(u32_at(rec, 0x1C).ok_or(RecordError::Header)?)
        .map_err(|_| RecordError::Header)?;
    if allocated != len
        || used > len
        || attrs_at < usa_end
        || attrs_at % 8 != 0
        || attrs_at.saturating_add(4) > used
    {
        return Err(RecordError::Header);
    }
    let base_ref = u64_at(rec, 0x20).ok_or(RecordError::Header)?;
    Ok(Header {
        seq: u16_at(rec, 0x10).ok_or(RecordError::Header)?,
        flags,
        attrs_at,
        used,
        base: (base_ref != 0).then(|| split_ref(base_ref)),
        number: if usa_at >= 0x30 {
            u32_at(rec, 0x2C)
        } else {
            None
        },
    })
}

/// A non-resident attribute's header fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NonResident<'a> {
    pub lowest_vcn: u64,
    pub highest_vcn: u64,
    /// Compression unit as a power of two of clusters; 0 = not compressed or
    /// sparse.
    pub unit: u8,
    pub allocated: u64,
    pub size: u64,
    pub initialized: u64,
    /// Present for compressed and sparse attributes.
    pub compressed: Option<u64>,
    /// The mapping pairs, ending at the attribute's end.
    pub runs: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Body<'a> {
    Resident(&'a [u8]),
    NonResident(NonResident<'a>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Attr<'a> {
    pub ty: u32,
    pub flags: u16,
    /// UTF-16LE bytes of the attribute's name; empty for an unnamed one.
    pub name: &'a [u8],
    pub body: Body<'a>,
}

impl Attr<'_> {
    pub fn is_unnamed(&self) -> bool {
        self.name.is_empty()
    }
}

/// An attribute that does not fit in its record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AttrError;

/// The attributes of a record, in order, up to the end marker. After an error
/// nothing more is returned.
pub(crate) struct Attrs<'a> {
    rec: &'a [u8],
    pos: usize,
    end: usize,
    done: bool,
}

pub(crate) fn attrs<'a>(rec: &'a [u8], header: &Header) -> Attrs<'a> {
    Attrs {
        rec,
        pos: header.attrs_at,
        end: header.used.min(rec.len()),
        done: false,
    }
}

impl<'a> Attrs<'a> {
    fn parse(&self) -> Result<Option<(Attr<'a>, usize)>, AttrError> {
        let rec = self.rec;
        let at = self.pos;
        let Some(ty) = u32_at(rec, at) else {
            return Err(AttrError);
        };
        if ty == ty::END {
            return Ok(None);
        }
        let len = usize::try_from(u32_at(rec, at.saturating_add(4)).ok_or(AttrError)?)
            .map_err(|_| AttrError)?;
        let next = at.checked_add(len).ok_or(AttrError)?;
        if len < 0x18 || len % 8 != 0 || next > self.end {
            return Err(AttrError);
        }
        let this = rec.get(at..next).ok_or(AttrError)?;
        let non_resident = u8_at(this, 8).ok_or(AttrError)?;
        let name_len = usize::from(u8_at(this, 9).ok_or(AttrError)?);
        let name_at = usize::from(u16_at(this, 10).ok_or(AttrError)?);
        let flags = u16_at(this, 12).ok_or(AttrError)?;
        let name = if name_len == 0 {
            &[][..]
        } else {
            bytes_at(this, name_at, name_len.checked_mul(2).ok_or(AttrError)?).ok_or(AttrError)?
        };
        let body = match non_resident {
            0 => {
                let value_len =
                    usize::try_from(u32_at(this, 0x10).ok_or(AttrError)?).map_err(|_| AttrError)?;
                let value_at = usize::from(u16_at(this, 0x14).ok_or(AttrError)?);
                Body::Resident(bytes_at(this, value_at, value_len).ok_or(AttrError)?)
            }
            1 => {
                if len < 0x40 {
                    return Err(AttrError);
                }
                let runs_at = usize::from(u16_at(this, 0x20).ok_or(AttrError)?);
                let unit = u8_at(this, 0x22).ok_or(AttrError)?;
                if runs_at < 0x40 || runs_at > len {
                    return Err(AttrError);
                }
                Body::NonResident(NonResident {
                    lowest_vcn: u64_at(this, 0x10).ok_or(AttrError)?,
                    highest_vcn: u64_at(this, 0x18).ok_or(AttrError)?,
                    unit,
                    allocated: u64_at(this, 0x28).ok_or(AttrError)?,
                    size: u64_at(this, 0x30).ok_or(AttrError)?,
                    initialized: u64_at(this, 0x38).ok_or(AttrError)?,
                    compressed: if unit != 0 && len >= 0x48 {
                        u64_at(this, 0x40)
                    } else {
                        None
                    },
                    runs: this.get(runs_at..).ok_or(AttrError)?,
                })
            }
            _ => return Err(AttrError),
        };
        Ok(Some((
            Attr {
                ty,
                flags,
                name,
                body,
            },
            next,
        )))
    }
}

impl<'a> Iterator for Attrs<'a> {
    type Item = Result<Attr<'a>, AttrError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        // The end marker is optional when the attributes fill the record.
        if self.pos >= self.end {
            self.done = true;
            return None;
        }
        match self.parse() {
            Ok(Some((attr, next))) => {
                self.pos = next;
                Some(Ok(attr))
            }
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]
    use super::*;
    use crate::testkit::RecordBuilder;

    #[test]
    fn a_built_record_round_trips_through_fixups() {
        for size in [1024usize, 4096] {
            let built = RecordBuilder::new(size, 7).standard_info(0x20, 5).build();
            // Patched sectors are what the update sequence guards.
            for i in 1..size / 512 {
                assert_eq!(built[i * 512 - 2..i * 512], [0xA5, 0x5A][..], "stamped");
            }
            let mut rec = built.clone();
            assert_eq!(peek(&rec), Peek::Used);
            let h = fix_and_parse(&mut rec).unwrap();
            assert_eq!(h.seq, 7);
            assert!(!h.is_dir());
            assert!(h.base.is_none());
            let all: Vec<_> = attrs(&rec, &h).collect::<Result<_, _>>().unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].ty, ty::STANDARD_INFORMATION);
        }
    }

    #[test]
    fn a_torn_sector_is_a_fixup_error() {
        let mut rec = RecordBuilder::new(1024, 1).build();
        rec[510] ^= 0xFF;
        assert_eq!(fix_and_parse(&mut rec), Err(RecordError::Fixup));
        let mut rec = RecordBuilder::new(1024, 1).build();
        rec[1022] = 0;
        assert_eq!(fix_and_parse(&mut rec), Err(RecordError::Fixup));
    }

    #[test]
    fn header_lies_are_refused() {
        let good = RecordBuilder::new(1024, 1).standard_info(0x20, 0).build();
        let patch = |at: usize, bytes: &[u8]| {
            let mut r = good.clone();
            r[at..at + bytes.len()].copy_from_slice(bytes);
            r
        };
        for (what, mut rec) in [
            ("update count", patch(6, &9u16.to_le_bytes())),
            ("update offset low", patch(4, &8u16.to_le_bytes())),
            ("update offset odd", patch(4, &0x31u16.to_le_bytes())),
            ("update offset high", patch(4, &0x1F0u16.to_le_bytes())),
            ("attrs before usa", patch(0x14, &0x10u16.to_le_bytes())),
            ("attrs unaligned", patch(0x14, &0x39u16.to_le_bytes())),
            ("used beyond", patch(0x18, &5000u32.to_le_bytes())),
            ("allocated differs", patch(0x1C, &512u32.to_le_bytes())),
        ] {
            assert!(fix_and_parse(&mut rec).is_err(), "{what}");
        }
        // Odd sizes.
        let mut small = vec![0u8; 300];
        small[..4].copy_from_slice(b"FILE");
        assert!(fix_and_parse(&mut small).is_err());
        assert!(fix_and_parse(&mut []).is_err());
    }

    #[test]
    fn peek_tells_free_from_damaged() {
        let mut rec = RecordBuilder::new(1024, 1).build();
        assert_eq!(peek(&rec), Peek::Used);
        rec[0x16] = 0;
        assert_eq!(peek(&rec), Peek::Unused);
        assert_eq!(peek(&[0u8; 1024]), Peek::Unused);
        rec[..4].copy_from_slice(b"BAAD");
        assert_eq!(peek(&rec), Peek::Bad);
        rec[..4].copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(peek(&rec), Peek::Bad);
        assert_eq!(peek(&[]), Peek::Bad);
    }

    #[test]
    fn broken_attributes_stop_the_iteration() {
        let good = RecordBuilder::new(1024, 1)
            .standard_info(0x20, 0)
            .file_name(5, 5, 1, "a.txt")
            .build();
        let mut rec = good.clone();
        let h = fix_and_parse(&mut rec).unwrap();
        assert_eq!(attrs(&rec, &h).count(), 2);
        // Length zero, unaligned, too long.
        for bad_len in [0u32, 0x19, 0x7FFF_FFF8, 8] {
            let mut r = rec.clone();
            r[h.attrs_at + 4..h.attrs_at + 8].copy_from_slice(&bad_len.to_le_bytes());
            let items: Vec<_> = attrs(&r, &h).collect();
            assert_eq!(items.len(), 1, "{bad_len:#x}");
            assert!(items[0].is_err());
        }
        // Resident value outside the attribute.
        let mut r = rec.clone();
        r[h.attrs_at + 0x10..h.attrs_at + 0x14].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert!(attrs(&r, &h).next().unwrap().is_err());
        // An attribute flag byte that is neither resident nor not.
        let mut r = rec;
        r[h.attrs_at + 8] = 7;
        assert!(attrs(&r, &h).next().unwrap().is_err());
    }
}
