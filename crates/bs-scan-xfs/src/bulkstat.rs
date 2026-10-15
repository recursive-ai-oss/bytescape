//! The bulk inode request and its reply, decoded with every offset and count
//! checked. The layouts are those of the kernel's `xfs_fs.h`
//! (`struct xfs_bulkstat_req`, `struct xfs_bulkstat`).
//!
//! The reply comes from the kernel, but is treated as hostile all the same: a
//! count the buffer cannot hold, inodes that do not increase, or a record with
//! an impossible version end the read with an error instead of a panic, an
//! overread or a read that never ends.

use crate::XfsError;
use crate::le::{u16_at, u32_at, u64_at};

/// Bytes of the request header (`struct xfs_bulk_ireq`).
pub(crate) const HEADER_LEN: usize = 64;
/// Bytes of one `struct xfs_bulkstat`.
pub(crate) const RECORD_LEN: usize = 192;

// Offsets in the request header.
pub(crate) const REQ_INO: usize = 0;
pub(crate) const REQ_FLAGS: usize = 8;
pub(crate) const REQ_ICOUNT: usize = 12;
pub(crate) const REQ_OCOUNT: usize = 16;

// Offsets in a record.
const BS_INO: usize = 0;
const BS_SIZE: usize = 8;
const BS_BLOCKS: usize = 16;
const BS_MTIME: usize = 40;
const BS_BLKSIZE: usize = 96;
const BS_NLINK: usize = 112;
const BS_MODE: usize = 132;

/// The fields of a bulk inode record that the reader uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BulkStat {
    /// The inode number.
    pub ino: u64,
    /// The file size in bytes.
    pub size: u64,
    /// The blocks the inode holds, in file system blocks.
    pub blocks: u64,
    /// The file system block size.
    pub block_size: u32,
    /// Modification time, whole seconds since the Unix epoch.
    pub mtime: i64,
    /// The link count.
    pub nlink: u32,
    /// Type and permission bits.
    pub mode: u16,
}

impl BulkStat {
    /// The bytes the inode holds on disk. Saturates: both factors come from
    /// the disk.
    pub fn allocated(&self) -> u64 {
        self.blocks.saturating_mul(u64::from(self.block_size))
    }
}

/// Decodes one record; `None` if it is too short.
pub(crate) fn parse_record(rec: &[u8]) -> Option<BulkStat> {
    if rec.len() < RECORD_LEN {
        return None;
    }
    let mtime = u64_at(rec, BS_MTIME)?;
    Some(BulkStat {
        ino: u64_at(rec, BS_INO)?,
        size: u64_at(rec, BS_SIZE)?,
        blocks: u64_at(rec, BS_BLOCKS)?,
        block_size: u32_at(rec, BS_BLKSIZE)?,
        mtime: i64::from_le_bytes(mtime.to_le_bytes()),
        nlink: u32_at(rec, BS_NLINK)?,
        mode: u16_at(rec, BS_MODE)?,
    })
}

/// The request header asking for up to `count` inodes after `after` (0 to
/// start). The buffer that follows it must hold `count` records.
pub(crate) fn request_header(after: u64, count: u32) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[REQ_INO..REQ_INO + 8].copy_from_slice(&after.to_le_bytes());
    h[REQ_FLAGS..REQ_FLAGS + 4].copy_from_slice(&0u32.to_le_bytes());
    h[REQ_ICOUNT..REQ_ICOUNT + 4].copy_from_slice(&count.to_le_bytes());
    h
}

/// What reading one reply showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyEnd {
    /// The reply held no record: the file system is exhausted.
    Empty,
    /// The inode number of the last record; the next request goes after it.
    Last(u64),
}

/// Reads the `ocount` records of a reply to a request that started at `after`.
/// The first inode number must not be below `after`, and the numbers must
/// increase from record to record.
pub(crate) fn read_reply(
    records: &[u8],
    ocount: u32,
    after: u64,
    each: &mut dyn FnMut(BulkStat),
) -> Result<ReplyEnd, XfsError> {
    let count = usize::try_from(ocount).map_err(|_| XfsError::Hostile("record count"))?;
    if count > records.len() / RECORD_LEN {
        return Err(XfsError::Hostile("more records than the buffer can hold"));
    }
    let mut last: Option<u64> = None;
    for chunk in records.as_chunks::<RECORD_LEN>().0.iter().take(count) {
        let rec = parse_record(chunk.as_slice()).ok_or(XfsError::Hostile("short record"))?;
        if rec.ino < after || last.is_some_and(|l| rec.ino <= l) {
            return Err(XfsError::Hostile("inode numbers do not increase"));
        }
        last = Some(rec.ino);
        each(rec);
    }
    Ok(last.map_or(ReplyEnd::Empty, ReplyEnd::Last))
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

    /// One record as the kernel lays it out.
    pub(crate) fn record(
        ino: u64,
        size: u64,
        blocks: u64,
        mtime: i64,
        nlink: u32,
        mode: u16,
    ) -> Vec<u8> {
        let mut r = vec![0u8; RECORD_LEN];
        r[BS_INO..BS_INO + 8].copy_from_slice(&ino.to_le_bytes());
        r[BS_SIZE..BS_SIZE + 8].copy_from_slice(&size.to_le_bytes());
        r[BS_BLOCKS..BS_BLOCKS + 8].copy_from_slice(&blocks.to_le_bytes());
        r[BS_MTIME..BS_MTIME + 8].copy_from_slice(&mtime.to_le_bytes());
        r[BS_BLKSIZE..BS_BLKSIZE + 4].copy_from_slice(&4096u32.to_le_bytes());
        r[BS_NLINK..BS_NLINK + 4].copy_from_slice(&nlink.to_le_bytes());
        r[BS_MODE..BS_MODE + 2].copy_from_slice(&mode.to_le_bytes());
        r
    }

    #[test]
    fn a_record_gives_its_fields() {
        let r = parse_record(&record(131, 12345, 4, 1_700_000_000, 2, 0o100_644)).unwrap();
        assert_eq!(
            r,
            BulkStat {
                ino: 131,
                size: 12345,
                blocks: 4,
                block_size: 4096,
                mtime: 1_700_000_000,
                nlink: 2,
                mode: 0o100_644_u16,
            }
        );
        assert_eq!(r.allocated(), 16_384);
        let neg = parse_record(&record(1, 0, 0, -86_400, 1, 0o040_755)).unwrap();
        assert_eq!(neg.mtime, -86_400);
        for cut in 0..RECORD_LEN {
            assert!(
                parse_record(&record(1, 0, 0, 0, 1, 0)[..cut]).is_none(),
                "{cut}"
            );
        }
    }

    #[test]
    fn the_size_on_disk_cannot_overflow() {
        let mut r = parse_record(&record(1, 0, 0, 0, 1, 0)).unwrap();
        r.blocks = u64::MAX;
        assert_eq!(r.allocated(), u64::MAX);
    }

    #[test]
    fn the_request_header_has_the_kernels_layout() {
        let h = request_header(77, 1024);
        assert_eq!(u64::from_le_bytes(h[0..8].try_into().unwrap()), 77);
        assert_eq!(u32::from_le_bytes(h[12..16].try_into().unwrap()), 1024);
        assert_eq!(
            u32::from_le_bytes(h[8..12].try_into().unwrap()),
            0,
            "no flags"
        );
        assert!(
            h[16..].iter().all(|&b| b == 0),
            "ocount, agno and the reserved fields"
        );
        assert_eq!(h.len(), 64);
    }

    #[test]
    fn a_good_reply_is_read_in_order_and_names_where_to_go_on() {
        let mut buf = Vec::new();
        for ino in [10u64, 11, 40] {
            buf.extend(record(ino, ino, 1, 0, 1, 0o100_600));
        }
        let mut seen = Vec::new();
        let end = read_reply(&buf, 3, 0, &mut |r| seen.push(r.ino)).unwrap();
        assert_eq!(end, ReplyEnd::Last(40));
        assert_eq!(seen, vec![10, 11, 40]);
        assert_eq!(
            read_reply(&[], 0, 40, &mut |_| ()).unwrap(),
            ReplyEnd::Empty
        );
        // Nothing before where the request started; the start itself is in.
        assert!(read_reply(&buf, 3, 40, &mut |_| ()).is_err());
        assert!(read_reply(&buf, 3, 11, &mut |_| ()).is_err());
        assert_eq!(
            read_reply(&buf, 3, 10, &mut |_| ()).unwrap(),
            ReplyEnd::Last(40)
        );
        assert_eq!(
            read_reply(&buf, 3, 9, &mut |_| ()).unwrap(),
            ReplyEnd::Last(40)
        );
    }

    #[test]
    fn nonsense_replies_are_errors() {
        let mut buf = Vec::new();
        for ino in [10u64, 11, 12] {
            buf.extend(record(ino, 0, 0, 0, 1, 0));
        }
        // More records claimed than the buffer holds.
        assert!(read_reply(&buf, 4, 0, &mut |_| ()).is_err());
        assert!(read_reply(&buf, u32::MAX, 0, &mut |_| ()).is_err());
        assert!(read_reply(&buf[..RECORD_LEN * 2 + 5], 3, 0, &mut |_| ()).is_err());
        // Inode numbers that repeat or go back.
        let mut twice = record(10, 0, 0, 0, 1, 0);
        twice.extend(record(10, 0, 0, 0, 1, 0));
        assert!(read_reply(&twice, 2, 0, &mut |_| ()).is_err());
        let mut back = record(10, 0, 0, 0, 1, 0);
        back.extend(record(9, 0, 0, 0, 1, 0));
        assert!(read_reply(&back, 2, 0, &mut |_| ()).is_err());
    }
}
