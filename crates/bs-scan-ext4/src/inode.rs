//! Inodes: the fixed fields of the on-disk record, reduced to what a size
//! tree needs.

use crate::le::{bytes_at, join64, u16_at, u32_at};
use crate::sb::Geometry;

pub(crate) const S_IFMT: u16 = 0xF000;
pub(crate) const S_IFDIR: u16 = 0x4000;
pub(crate) const S_IFREG: u16 = 0x8000;
pub(crate) const S_IFLNK: u16 = 0xA000;
const S_IFIFO: u16 = 0x1000;
const S_IFCHR: u16 = 0x2000;
const S_IFBLK: u16 = 0x6000;
const S_IFSOCK: u16 = 0xC000;

/// Inode flags (`i_flags`) that matter here.
pub(crate) const FL_HUGE_FILE: u32 = 0x0004_0000;
pub(crate) const FL_EXTENTS: u32 = 0x0008_0000;
pub(crate) const FL_INLINE_DATA: u32 = 0x1000_0000;

/// The inode is in use and its fields passed the checks.
pub(crate) const REC_IN_USE: u32 = 1;
/// The inode is in use but impossible (a type that does not exist, a size no
/// file can have).
pub(crate) const REC_BAD: u32 = 2;
/// The part of the inode table holding the inode could not be read.
pub(crate) const REC_UNREADABLE: u32 = 4;

/// What is kept of an inode that is in use: 32 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InodeRec {
    /// Logical size.
    pub size: u64,
    /// Allocated size in 512-byte units, as `st_blocks`.
    pub blocks512: u64,
    /// Modification time, seconds since the epoch.
    pub mtime: i64,
    /// `REC_*` bits. Zero means the inode is free.
    pub state: u32,
    pub mode: u16,
    pub links: u16,
}

impl InodeRec {
    pub fn is_in_use(&self) -> bool {
        self.state & REC_IN_USE != 0
    }

    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
}

/// The type bits of a mode, if they name a real type.
pub(crate) fn valid_type(mode: u16) -> bool {
    matches!(
        mode & S_IFMT,
        S_IFIFO | S_IFCHR | S_IFDIR | S_IFBLK | S_IFREG | S_IFLNK | S_IFSOCK
    )
}

/// The outcome of looking at one inode record.
pub(crate) enum Parsed {
    /// Not in use (no links).
    Free,
    /// In use but nonsense; the entry that names it is reported unreadable.
    Bad,
    Used(InodeRec),
}

/// Reads the fixed fields of an inode record. `raw` is the whole record
/// (`inode_size` bytes).
pub(crate) fn parse_inode(raw: &[u8], geo: &Geometry) -> Parsed {
    let (Some(mode), Some(links)) = (u16_at(raw, 0x00), u16_at(raw, 0x1A)) else {
        return Parsed::Bad;
    };
    if links == 0 {
        return Parsed::Free;
    }
    let (Some(size_lo), Some(blocks_lo), Some(flags), Some(mtime_lo), Some(size_hi)) = (
        u32_at(raw, 0x04),
        u32_at(raw, 0x1C),
        u32_at(raw, 0x20),
        u32_at(raw, 0x10),
        u32_at(raw, 0x6C),
    ) else {
        return Parsed::Bad;
    };
    if !valid_type(mode) {
        return Parsed::Bad;
    }
    let size = if geo.has_large_dir() || mode & S_IFMT == S_IFREG {
        join64(size_lo, size_hi)
    } else {
        u64::from(size_lo)
    };
    if i64::try_from(size).is_err() {
        return Parsed::Bad;
    }

    // Blocks are counted in 512-byte units; with the huge-file feature the
    // count has 16 more bits, and an inode flag says the unit is a whole
    // file system block instead.
    let mut blocks512 = u64::from(blocks_lo);
    if geo.has_huge_file() {
        let Some(hi) = u16_at(raw, 0x74) else {
            return Parsed::Bad;
        };
        blocks512 |= u64::from(hi).rotate_left(32);
        if flags & FL_HUGE_FILE != 0 {
            let factor = u64::from(geo.block_size).checked_div(512).unwrap_or(1);
            match blocks512.checked_mul(factor) {
                Some(b) => blocks512 = b,
                None => return Parsed::Bad,
            }
        }
    }
    if blocks512.checked_mul(512).is_none() {
        return Parsed::Bad;
    }

    Parsed::Used(InodeRec {
        size,
        blocks512,
        mtime: mtime_seconds(raw, mtime_lo),
        state: REC_IN_USE,
        mode,
        links,
    })
}

/// The modification time in seconds. The low field is a signed 32-bit count;
/// the two low bits of the extra field extend it past 2038 (and, with the
/// sign, back before 1901).
fn mtime_seconds(raw: &[u8], lo: u32) -> i64 {
    let mut seconds = i64::from(i32::from_le_bytes(lo.to_le_bytes()));
    let extra_size = u16_at(raw, 0x80).map_or(0, usize::from);
    // The extra field lies at 0x88; it exists when the inode's extra area
    // reaches past it (offsets are from the start of the extra area, 128).
    if extra_size >= 12
        && let Some(extra) = u32_at(raw, 0x88)
    {
        seconds = seconds.saturating_add(i64::from(extra & 3).rotate_left(32));
    }
    seconds
}

/// The 60 bytes of `i_block`: extent root, block pointers, fast symlink
/// text or inline data.
pub(crate) fn i_block(raw: &[u8]) -> Option<&[u8]> {
    bytes_at(raw, 0x28, 60)
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

    fn raw(mode: u16, links: u16) -> Vec<u8> {
        let mut r = vec![0u8; 256];
        r[0..2].copy_from_slice(&mode.to_le_bytes());
        r[0x1A..0x1C].copy_from_slice(&links.to_le_bytes());
        r
    }

    #[test]
    fn free_bad_and_used() {
        let geo = crate::testfs::geometry_4k();
        assert!(matches!(parse_inode(&raw(S_IFREG, 0), &geo), Parsed::Free));
        assert!(matches!(parse_inode(&[0u8; 8], &geo), Parsed::Bad));
        assert!(matches!(parse_inode(&raw(0x3000, 1), &geo), Parsed::Bad));
        assert!(matches!(parse_inode(&raw(0, 1), &geo), Parsed::Bad));
        let mut r = raw(S_IFREG | 0o644, 1);
        r[0x04..0x08].copy_from_slice(&100u32.to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&8u32.to_le_bytes());
        let Parsed::Used(rec) = parse_inode(&r, &geo) else {
            panic!("expected used");
        };
        assert_eq!((rec.size, rec.blocks512), (100, 8));
        assert!(rec.is_in_use() && !rec.is_dir());
    }

    #[test]
    fn size_high_counts_for_files_only() {
        let geo = crate::testfs::geometry_4k();
        let mut r = raw(S_IFREG, 1);
        r[0x6C..0x70].copy_from_slice(&1u32.to_le_bytes());
        let Parsed::Used(f) = parse_inode(&r, &geo) else {
            panic!()
        };
        assert_eq!(f.size, 1 << 32);
        let mut r = raw(S_IFDIR, 1);
        r[0x6C..0x70].copy_from_slice(&1u32.to_le_bytes());
        let Parsed::Used(d) = parse_inode(&r, &geo) else {
            panic!()
        };
        assert_eq!(d.size, 0);
        // A size no file can have.
        let mut r = raw(S_IFREG, 1);
        r[0x6C..0x70].copy_from_slice(&u32::MAX.to_le_bytes());
        r[0x04..0x08].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(parse_inode(&r, &geo), Parsed::Bad));
    }

    #[test]
    fn mtime_uses_the_epoch_bits_only_when_the_extra_area_has_them() {
        let mut r = raw(S_IFREG, 1);
        r[0x10..0x14].copy_from_slice(&1000u32.to_le_bytes());
        r[0x88..0x8C].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(mtime_seconds(&r, 1000), 1000);
        r[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        assert_eq!(mtime_seconds(&r, 1000), 1000 + (1 << 32));
        // Before 1970: the low field is signed.
        let lo = u32::from_le_bytes((-5i32).to_le_bytes());
        r[0x88..0x8C].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(mtime_seconds(&r, lo), -5);
    }
}
