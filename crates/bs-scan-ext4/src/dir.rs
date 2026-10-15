//! Directory contents: entries in blocks, and the inline form that keeps a
//! small directory inside its inode.

use crate::le::{bytes_at, u8_at, u16_at, u32_at};

/// A directory entry as found on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Dirent<'a> {
    pub inode: u32,
    /// The file type byte, or 0 when the file system does not keep one.
    pub file_type: u8,
    pub name: &'a [u8],
}

/// What a walk over a block found besides ordinary entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event<'a> {
    Entry(Dirent<'a>),
    /// The `..` entry: the inode of the parent directory.
    DotDot(u32),
    /// An entry with a name that cannot exist.
    BadName(u32),
}

/// The entry lengths in a block do not fit it. Entries before the bad one
/// were reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockCorrupt {
    pub offset: usize,
}

/// Decodes the length field. Blocks of 64 KiB cannot store their own length
/// in 16 bits, so zero and 65535 mean "the whole block" and the two low
/// bits carry the top bits.
fn rec_len_from_disk(raw: u16, block_size: usize) -> usize {
    let raw = usize::from(raw);
    if block_size < 65536 {
        raw
    } else if raw == 0 || raw == 65535 {
        65536
    } else {
        (raw & 65532) | ((raw & 3) << 16)
    }
}

/// Walks the entries of one directory block (or inline area).
///
/// `block_size` is the file system's, used only to decode lengths.
/// `with_dots` says the area starts with `.` and `..` entries. `has_types`
/// says the type byte means something. `visit` returns `false` to stop.
pub(crate) fn walk_block<'a>(
    buf: &'a [u8],
    block_size: usize,
    with_dots: bool,
    has_types: bool,
    mut visit: impl FnMut(Event<'a>) -> bool,
) -> Result<(), BlockCorrupt> {
    let mut offset = 0usize;
    let mut position = 0usize;
    while offset < buf.len() {
        let corrupt = BlockCorrupt { offset };
        let (Some(inode), Some(raw_len), Some(name_len), Some(file_type)) = (
            u32_at(buf, offset),
            u16_at(buf, offset.saturating_add(4)),
            u8_at(buf, offset.saturating_add(6)),
            u8_at(buf, offset.saturating_add(7)),
        ) else {
            return Err(corrupt);
        };
        let rec_len = rec_len_from_disk(raw_len, block_size);
        let name_len = usize::from(name_len);
        let end = offset.checked_add(rec_len).ok_or(corrupt)?;
        // The entry must be a whole number of words, hold its own name and
        // stay inside the block. The shortest possible is the 8-byte header.
        if rec_len < 8
            || rec_len.checked_rem(4) != Some(0)
            || rec_len < name_len.saturating_add(8)
            || end > buf.len()
        {
            return Err(corrupt);
        }
        if inode != 0 {
            let name = bytes_at(buf, offset.saturating_add(8), name_len).ok_or(corrupt)?;
            let event = if with_dots && position == 0 && name == b"." {
                None
            } else if with_dots && position == 1 && name == b".." {
                Some(Event::DotDot(inode))
            } else if name.is_empty()
                || name == b"."
                || name == b".."
                || name.iter().any(|&b| b == b'/' || b == 0)
            {
                Some(Event::BadName(inode))
            } else {
                Some(Event::Entry(Dirent {
                    inode,
                    file_type: if has_types { file_type } else { 0 },
                    name,
                }))
            };
            if let Some(event) = event
                && !visit(event)
            {
                return Ok(());
            }
            position = position.saturating_add(1);
        }
        offset = end;
    }
    Ok(())
}

const XATTR_MAGIC: u32 = 0xEA02_0000;
/// The attribute namespace that holds `data`: "system.".
const XATTR_INDEX_SYSTEM: u8 = 7;
const XATTR_ENTRY_HEADER: usize = 16;

/// The `system.data` attribute stored in an inode's spare room: the part of
/// an inline directory that did not fit in `i_block`.
pub(crate) fn inline_tail(raw_inode: &[u8]) -> Option<&[u8]> {
    let extra = usize::from(u16_at(raw_inode, 0x80)?);
    let start = 128usize.checked_add(extra)?;
    if u32_at(raw_inode, start)? != XATTR_MAGIC {
        return None;
    }
    let entries = start.checked_add(4)?;
    let mut at = entries;
    // Every entry takes at least its 16-byte header, so this ends.
    for _ in 0..raw_inode.len().checked_div(XATTR_ENTRY_HEADER)? {
        if u32_at(raw_inode, at)? == 0 {
            return None;
        }
        let name_len = usize::from(u8_at(raw_inode, at)?);
        let index = u8_at(raw_inode, at.checked_add(1)?)?;
        let value_offset = usize::from(u16_at(raw_inode, at.checked_add(2)?)?);
        let value_inode = u32_at(raw_inode, at.checked_add(4)?)?;
        let value_size = usize::try_from(u32_at(raw_inode, at.checked_add(8)?)?).ok()?;
        let name = bytes_at(raw_inode, at.checked_add(XATTR_ENTRY_HEADER)?, name_len)?;
        if index == XATTR_INDEX_SYSTEM && name == b"data" && value_inode == 0 {
            return bytes_at(raw_inode, entries.checked_add(value_offset)?, value_size);
        }
        let padded = name_len.checked_add(3)? & !3;
        at = at.checked_add(XATTR_ENTRY_HEADER)?.checked_add(padded)?;
    }
    None
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

    fn put(buf: &mut [u8], at: usize, inode: u32, rec_len: u16, name: &[u8], ty: u8) {
        buf[at..at + 4].copy_from_slice(&inode.to_le_bytes());
        buf[at + 4..at + 6].copy_from_slice(&rec_len.to_le_bytes());
        buf[at + 6] = name.len() as u8;
        buf[at + 7] = ty;
        buf[at + 8..at + 8 + name.len()].copy_from_slice(name);
    }

    fn block() -> Vec<u8> {
        let mut b = vec![0u8; 1024];
        put(&mut b, 0, 2, 12, b".", 2);
        put(&mut b, 12, 2, 12, b"..", 2);
        put(&mut b, 24, 12, 16, b"hello", 1);
        put(&mut b, 40, 0, 12, b"gone", 1); // deleted entry
        put(&mut b, 52, 13, 1024 - 52, b"last\xff", 7);
        b
    }

    fn collect(b: &[u8], dots: bool) -> (Vec<String>, Result<(), BlockCorrupt>) {
        let mut seen = Vec::new();
        let r = walk_block(b, 1024, dots, true, |e| {
            seen.push(match e {
                Event::Entry(d) => format!(
                    "{}:{}:{}",
                    d.inode,
                    d.file_type,
                    String::from_utf8_lossy(d.name)
                ),
                Event::DotDot(i) => format!("dotdot {i}"),
                Event::BadName(i) => format!("bad {i}"),
            });
            true
        });
        (seen, r)
    }

    #[test]
    fn entries_dots_and_deleted_slots() {
        let (seen, r) = collect(&block(), true);
        assert!(r.is_ok());
        assert_eq!(seen, ["dotdot 2", "12:1:hello", "13:7:last\u{fffd}"]);
    }

    #[test]
    fn dots_are_only_special_where_they_belong() {
        let (seen, _) = collect(&block(), false);
        assert_eq!(seen[0], "bad 2");
        assert_eq!(seen[1], "bad 2");
    }

    #[test]
    fn every_bad_length_is_corrupt_not_a_panic() {
        let base = block();
        for (at, rec_len) in [
            (24usize, 0u16),
            (24, 4),
            (24, 6),
            (24, 5000),
            (24, 9),
            (52, 1000),
            (0, 1),
        ] {
            let mut b = base.clone();
            b[at + 4..at + 6].copy_from_slice(&rec_len.to_le_bytes());
            let (_, r) = collect(&b, true);
            assert!(r.is_err(), "{at} {rec_len}");
        }
        // A name longer than its record.
        let mut b = base.clone();
        b[24 + 6] = 200;
        assert!(collect(&b, true).1.is_err());
        // A header cut off by the end of the area.
        assert!(collect(&base[..1020], true).1.is_err());
        // Names with a slash or NUL, and empty ones.
        let mut b = base.clone();
        b[24 + 8] = b'/';
        assert_eq!(collect(&b, true).0[1], "bad 12");
        let mut b = base.clone();
        b[24 + 6] = 0;
        assert_eq!(collect(&b, true).0[1], "bad 12");
    }

    #[test]
    fn a_visitor_can_stop_the_walk() {
        let mut n = 0;
        let r = walk_block(&block(), 1024, true, true, |_| {
            n += 1;
            n < 2
        });
        assert!(r.is_ok());
        assert_eq!(n, 2);
    }

    #[test]
    fn long_block_lengths() {
        assert_eq!(rec_len_from_disk(0, 65536), 65536);
        assert_eq!(rec_len_from_disk(65535, 65536), 65536);
        assert_eq!(rec_len_from_disk(12, 65536), 12);
        assert_eq!(rec_len_from_disk(12, 4096), 12);
        assert_eq!(rec_len_from_disk(0, 4096), 0);
    }

    #[test]
    fn inline_tail_found_and_bounds_checked() {
        let mut raw = vec![0u8; 256];
        raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        // Attributes begin at 160: magic, then one entry.
        raw[160..164].copy_from_slice(&XATTR_MAGIC.to_le_bytes());
        let e = 164;
        raw[e] = 4; // name length
        raw[e + 1] = XATTR_INDEX_SYSTEM;
        raw[e + 2..e + 4].copy_from_slice(&40u16.to_le_bytes()); // value offset
        raw[e + 8..e + 12].copy_from_slice(&8u32.to_le_bytes()); // value size
        raw[e + 16..e + 20].copy_from_slice(b"data");
        raw[164 + 40..164 + 48].copy_from_slice(b"ABCDEFGH");
        assert_eq!(inline_tail(&raw), Some(&b"ABCDEFGH"[..]));
        // A value running off the inode.
        raw[e + 8..e + 12].copy_from_slice(&5000u32.to_le_bytes());
        assert_eq!(inline_tail(&raw), None);
        raw[e + 8..e + 12].copy_from_slice(&8u32.to_le_bytes());
        raw[e + 2..e + 4].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(inline_tail(&raw), None);
        // No attributes, wrong magic, extra size past the inode.
        assert_eq!(inline_tail(&[0u8; 256]), None);
        let mut far = vec![0u8; 256];
        far[0x80..0x82].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(inline_tail(&far), None);
    }
}
