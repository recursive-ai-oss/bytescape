//! Windows directory-information records, read from plain bytes.
//!
//! `GetFileInformationByHandleEx` fills a buffer with a chain of
//! variable-length records. Nothing here calls Windows: the parsers take a
//! byte slice, so they are built and tested on every platform, with every
//! offset and length checked, because the buffer's contents come from
//! whichever filesystem driver answered.

#![cfg_attr(not(windows), allow(dead_code))]

/// `FILE_ATTRIBUTE_*` values the walker looks at.
pub mod attr {
    pub const DIRECTORY: u32 = 0x10;
    pub const SPARSE_FILE: u32 = 0x200;
    pub const REPARSE_POINT: u32 = 0x400;
    pub const COMPRESSED: u32 = 0x800;
    pub const OFFLINE: u32 = 0x1000;
    pub const RECALL_ON_OPEN: u32 = 0x4_0000;
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
}

const TAG_SYMLINK: u32 = 0xA000_000C;
/// `IO_REPARSE_TAG_CLOUD` and the variants `_1` to `_F`, which differ in bits
/// 12 to 15.
const TAG_CLOUD: u32 = 0x9000_001A;
const TAG_CLOUD_MASK: u32 = 0xFFFF_0FFF;

/// What a directory record is, for the walker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryClass {
    /// An ordinary file.
    File,
    /// An ordinary directory the walker may open and list.
    Dir,
    /// A symbolic link (file or directory). Never followed.
    Symlink,
    /// A junction, mount point or other reparse point. Never followed.
    Other,
}

/// What the walker does with a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Classified {
    pub class: EntryClass,
    /// The entry is a cloud placeholder: sizes come from the listing only and
    /// the entry is never opened.
    pub placeholder: bool,
    /// For a directory: it may be opened and listed. Reparse points and
    /// placeholder directories may not, because listing a placeholder folder
    /// can download its contents.
    pub descend: bool,
    /// The entry is a reparse point that was not followed.
    pub not_followed: bool,
}

pub fn is_cloud_tag(tag: u32) -> bool {
    tag & TAG_CLOUD_MASK == TAG_CLOUD
}

/// Decides what to do with a record from its attributes and reparse tag.
pub fn classify(attributes: u32, reparse_tag: u32) -> Classified {
    let is_dir = attributes & attr::DIRECTORY != 0;
    let reparse = attributes & attr::REPARSE_POINT != 0;
    let placeholder =
        attributes & (attr::RECALL_ON_DATA_ACCESS | attr::RECALL_ON_OPEN | attr::OFFLINE) != 0
            || (reparse && is_cloud_tag(reparse_tag));
    let class = match (reparse, is_dir) {
        // Placeholders are counted for what they say they are.
        (true, true) if placeholder => EntryClass::Dir,
        (true, _) if reparse_tag == TAG_SYMLINK => EntryClass::Symlink,
        (true, true) => EntryClass::Other,
        (_, true) => EntryClass::Dir,
        _ => EntryClass::File,
    };
    Classified {
        class,
        placeholder,
        descend: class == EntryClass::Dir && !placeholder && !reparse,
        not_followed: reparse,
    }
}

/// The producer flags for a classified record. `estimated` says the record
/// came from a listing that gave no allocation size, so its allocated size
/// was rounded up from the logical size: the tree then marks the entry and
/// every directory above it, and no total built on the guess looks measured.
pub fn entry_flags(class: &Classified, estimated: bool) -> bs_tree::NodeFlags {
    use bs_tree::NodeFlags;
    let mut flags = NodeFlags::EMPTY;
    if class.placeholder {
        flags |= NodeFlags::CLOUD_PLACEHOLDER;
    }
    if class.not_followed {
        flags |= NodeFlags::NOT_FOLLOWED;
    }
    if estimated {
        flags |= NodeFlags::ESTIMATED_SIZE;
    }
    flags
}

/// One record of a directory listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirRecord {
    /// Size in bytes (`EndOfFile`).
    pub logical: u64,
    /// Size on disk (`AllocationSize`).
    pub allocated: u64,
    /// Last write time as Unix seconds.
    pub mtime: i64,
    pub attributes: u32,
    /// The reparse tag, when the attributes say there is one.
    pub reparse_tag: u32,
    /// The file ID, or `None` where the filesystem has none (zero).
    pub file_id: Option<u128>,
    /// Where the UTF-16 name starts and its length, in bytes, in the buffer.
    pub name_at: usize,
    pub name_len: usize,
}

/// Why a listing buffer could not be read to its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListError {
    Truncated,
    BadOffset,
    BadName,
}

/// Which record layout a buffer holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// `FILE_ID_EXTD_DIR_INFO`: 128-bit file ID, reparse tag.
    Extended,
    /// `FILE_ID_BOTH_DIR_INFO`: 64-bit file ID, short name.
    Both,
}

struct Offsets {
    last_write: usize,
    end_of_file: usize,
    allocation: usize,
    attributes: usize,
    name_length: usize,
    ea_size: usize,
    reparse_tag: Option<usize>,
    file_id: usize,
    file_id_len: usize,
    name: usize,
}

const EXTENDED: Offsets = Offsets {
    last_write: 24,
    end_of_file: 40,
    allocation: 48,
    attributes: 56,
    name_length: 60,
    ea_size: 64,
    reparse_tag: Some(68),
    file_id: 72,
    file_id_len: 16,
    name: 88,
};

const BOTH: Offsets = Offsets {
    last_write: 24,
    end_of_file: 40,
    allocation: 48,
    attributes: 56,
    name_length: 60,
    ea_size: 64,
    reparse_tag: None,
    file_id: 96,
    file_id_len: 8,
    name: 104,
};

impl Layout {
    fn offsets(self) -> &'static Offsets {
        match self {
            Layout::Extended => &EXTENDED,
            Layout::Both => &BOTH,
        }
    }
}

fn u32_at(buf: &[u8], at: usize) -> Result<u32, ListError> {
    let end = at.checked_add(4).ok_or(ListError::Truncated)?;
    buf.get(at..end)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(ListError::Truncated)
}

fn i64_at(buf: &[u8], at: usize) -> Result<i64, ListError> {
    let end = at.checked_add(8).ok_or(ListError::Truncated)?;
    buf.get(at..end)
        .and_then(|s| s.try_into().ok())
        .map(i64::from_le_bytes)
        .ok_or(ListError::Truncated)
}

/// Seconds between 1601-01-01 and 1970-01-01.
const EPOCH_GAP_SECS: i64 = 11_644_473_600;

/// A Windows `FILETIME` (100 ns ticks since 1601) as Unix seconds.
pub fn filetime_to_unix(ticks: i64) -> i64 {
    ticks.div_euclid(10_000_000).saturating_sub(EPOCH_GAP_SECS)
}

/// Iterates the records of a filled buffer. After an error it yields nothing
/// more.
pub(crate) struct Records<'a> {
    buf: &'a [u8],
    layout: Layout,
    pos: Option<usize>,
}

pub(crate) fn records(buf: &[u8], layout: Layout) -> Records<'_> {
    Records {
        buf,
        layout,
        pos: if buf.is_empty() { None } else { Some(0) },
    }
}

impl Iterator for Records<'_> {
    type Item = Result<DirRecord, ListError>;

    fn next(&mut self) -> Option<Self::Item> {
        let pos = self.pos?;
        let item = self.parse(pos);
        match item {
            Ok((record, next)) => {
                self.pos = next;
                Some(Ok(record))
            }
            Err(e) => {
                self.pos = None;
                Some(Err(e))
            }
        }
    }
}

impl Records<'_> {
    fn parse(&self, pos: usize) -> Result<(DirRecord, Option<usize>), ListError> {
        let o = self.layout.offsets();
        let rec = self.buf.get(pos..).ok_or(ListError::Truncated)?;
        if rec.len() < o.name {
            return Err(ListError::Truncated);
        }
        let next_offset = u32_at(rec, 0)? as usize;
        let name_len = u32_at(rec, o.name_length)? as usize;
        let name_end = o.name.checked_add(name_len).ok_or(ListError::BadName)?;
        // The name is UTF-16, inside this record and inside the buffer.
        if !name_len.is_multiple_of(2) || name_end > rec.len() {
            return Err(ListError::BadName);
        }
        if next_offset != 0 && (next_offset < name_end || next_offset > rec.len()) {
            return Err(ListError::BadOffset);
        }
        let attributes = u32_at(rec, o.attributes)?;
        let reparse_tag = match o.reparse_tag {
            Some(at) => u32_at(rec, at)?,
            // The older layout keeps the tag where the extended-attribute size
            // would be.
            None if attributes & attr::REPARSE_POINT != 0 => u32_at(rec, o.ea_size)?,
            None => 0,
        };
        let id_bytes = rec
            .get(o.file_id..o.file_id + o.file_id_len)
            .ok_or(ListError::Truncated)?;
        let mut id = 0u128;
        for (i, b) in id_bytes.iter().enumerate() {
            id |= u128::from(*b) << (8 * i);
        }
        let record = DirRecord {
            logical: i64_at(rec, o.end_of_file)?.max(0) as u64,
            allocated: i64_at(rec, o.allocation)?.max(0) as u64,
            mtime: filetime_to_unix(i64_at(rec, o.last_write)?),
            attributes,
            reparse_tag,
            file_id: (id != 0).then_some(id),
            name_at: pos + o.name,
            name_len,
        };
        let next = (next_offset != 0).then(|| pos + next_offset);
        Ok((record, next))
    }
}

/// The UTF-16 name of a record as little-endian code units.
pub(crate) fn name_units(buf: &[u8], rec: &DirRecord) -> impl Iterator<Item = u16> {
    buf.get(rec.name_at..rec.name_at + rec.name_len)
        .unwrap_or(&[])
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
}

/// Whether a listed name is a single, non-empty path component: no NUL, no
/// backslash and no slash. Directory listings never contain such names, but
/// the walker opens children by name relative to their parent, and a name
/// with a separator in it would reach somewhere else; so a name that fails
/// this is recorded as damaged and never opened.
pub fn is_plain_component(units: &[u16]) -> bool {
    const BACKSLASH: u16 = b'\\' as u16;
    const SLASH: u16 = b'/' as u16;
    !units.is_empty()
        && !units
            .iter()
            .any(|&u| u == 0 || u == BACKSLASH || u == SLASH)
}

/// Joins a directory path and a name with exactly one backslash between
/// them. A volume root (`\\?\C:\`) already ends in one, and verbatim paths
/// are not normalized by the system, so a doubled separator would name a
/// different (nonexistent) path.
pub(crate) fn join_path(dir: &[u16], name: &[u16]) -> Vec<u16> {
    const BACKSLASH: u16 = b'\\' as u16;
    let mut out = Vec::with_capacity(dir.len() + name.len() + 1);
    out.extend_from_slice(dir);
    if !matches!(out.last(), None | Some(&BACKSLASH)) {
        out.push(BACKSLASH);
    }
    out.extend_from_slice(name);
    out
}

/// Appends UTF-16 text to `out` as WTF-8: UTF-8, except that an unpaired
/// surrogate (which Windows file names may contain) is written as the three
/// bytes its code point would take, so no name is changed or lost.
pub fn wtf8_from_utf16(units: impl IntoIterator<Item = u16>, out: &mut Vec<u8>) {
    let mut units = units.into_iter().peekable();
    while let Some(u) = units.next() {
        let code = if (0xD800..0xDC00).contains(&u) {
            match units.peek() {
                Some(&low) if (0xDC00..0xE000).contains(&low) => {
                    units.next();
                    0x1_0000 + ((u32::from(u) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
                }
                _ => u32::from(u),
            }
        } else {
            u32::from(u)
        };
        push_code(code, out);
    }
}

/// The UTF-16 units of WTF-8 text, the inverse of [`wtf8_from_utf16`]. Bytes
/// that are not well-formed (the text comes from a tree, which a damaged
/// disk can fill with anything) become U+FFFD one at a time.
pub fn utf16_from_wtf8(bytes: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0usize;
    while let Some(&lead) = bytes.get(at) {
        let (len, first) = match lead {
            0..=0x7F => (1usize, u32::from(lead)),
            0xC2..=0xDF => (2, u32::from(lead & 0x1F)),
            0xE0..=0xEF => (3, u32::from(lead & 0x0F)),
            0xF0..=0xF4 => (4, u32::from(lead & 0x07)),
            _ => (0, 0),
        };
        let mut code = first;
        let mut ok = len != 0;
        for k in 1..len {
            match bytes.get(at.saturating_add(k)) {
                Some(&c) if c & 0xC0 == 0x80 => code = (code << 6) | u32::from(c & 0x3F),
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        let shortest = match len {
            2 => 0x80,
            3 => 0x800,
            4 => 0x1_0000,
            _ => 0,
        };
        if !ok || code < shortest || code > 0x10_FFFF {
            out.push(0xFFFD);
            at = at.saturating_add(1);
            continue;
        }
        at = at.saturating_add(len);
        if code >= 0x1_0000 {
            let c = code - 0x1_0000;
            out.push(0xD800 + (c >> 10) as u16);
            out.push(0xDC00 + (c & 0x3FF) as u16);
        } else {
            out.push(code as u16);
        }
    }
    out
}

fn push_code(code: u32, out: &mut Vec<u8>) {
    if code < 0x80 {
        out.push(code as u8);
    } else if code < 0x800 {
        out.push(0xC0 | (code >> 6) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    } else if code < 0x1_0000 {
        out.push(0xE0 | (code >> 12) as u8);
        out.push(0x80 | ((code >> 6) & 0x3F) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    } else {
        out.push(0xF0 | (code >> 18) as u8);
        out.push(0x80 | ((code >> 12) & 0x3F) as u8);
        out.push(0x80 | ((code >> 6) & 0x3F) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Builds one record in `layout`, `next` bytes long including padding (0 for
    /// the last record).
    #[allow(clippy::too_many_arguments)]
    fn build(
        layout: Layout,
        name: &str,
        attributes: u32,
        tag: u32,
        id: u128,
        eof: i64,
        alloc: i64,
        write: i64,
        pad_to: usize,
    ) -> Vec<u8> {
        let o = layout.offsets();
        let name16: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut rec = vec![0u8; o.name + name16.len()];
        rec[o.last_write..o.last_write + 8].copy_from_slice(&write.to_le_bytes());
        rec[o.end_of_file..o.end_of_file + 8].copy_from_slice(&eof.to_le_bytes());
        rec[o.allocation..o.allocation + 8].copy_from_slice(&alloc.to_le_bytes());
        rec[o.attributes..o.attributes + 4].copy_from_slice(&attributes.to_le_bytes());
        rec[o.name_length..o.name_length + 4].copy_from_slice(&(name16.len() as u32).to_le_bytes());
        match o.reparse_tag {
            Some(at) => rec[at..at + 4].copy_from_slice(&tag.to_le_bytes()),
            None => rec[o.ea_size..o.ea_size + 4].copy_from_slice(&tag.to_le_bytes()),
        }
        let id_bytes = id.to_le_bytes();
        rec[o.file_id..o.file_id + o.file_id_len].copy_from_slice(&id_bytes[..o.file_id_len]);
        rec[o.name..].copy_from_slice(&name16);
        if pad_to > rec.len() {
            rec.resize(pad_to, 0);
        }
        rec
    }

    fn chain(parts: Vec<Vec<u8>>) -> Vec<u8> {
        let mut out = Vec::new();
        let last = parts.len() - 1;
        for (i, mut part) in parts.into_iter().enumerate() {
            // Records start 8-byte aligned.
            while part.len() % 8 != 0 {
                part.push(0);
            }
            if i != last {
                let len = part.len() as u32;
                part[0..4].copy_from_slice(&len.to_le_bytes());
            }
            out.extend(part);
        }
        out
    }

    #[test]
    fn reads_the_extended_layout() {
        let write = (1_700_000_000 + EPOCH_GAP_SECS) * 10_000_000;
        let buf = chain(vec![
            build(
                Layout::Extended,
                "a.txt",
                0x20,
                0,
                0xAB,
                100,
                4096,
                write,
                0,
            ),
            build(
                Layout::Extended,
                "dir",
                attr::DIRECTORY,
                0,
                (1u128 << 100) | 7,
                0,
                0,
                write,
                0,
            ),
        ]);
        let recs: Vec<_> = records(&buf, Layout::Extended)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(
            (recs[0].logical, recs[0].allocated, recs[0].mtime),
            (100, 4096, 1_700_000_000)
        );
        assert_eq!(recs[0].file_id, Some(0xAB));
        assert_eq!(recs[1].file_id, Some((1u128 << 100) | 7));
        let name: Vec<u16> = name_units(&buf, &recs[0]).collect();
        assert_eq!(String::from_utf16(&name).unwrap(), "a.txt");
    }

    #[test]
    fn reads_the_older_layout_with_the_tag_in_the_ea_field() {
        let buf = chain(vec![build(
            Layout::Both,
            "link",
            attr::REPARSE_POINT | attr::DIRECTORY,
            TAG_SYMLINK,
            5,
            0,
            0,
            0,
            0,
        )]);
        let rec = records(&buf, Layout::Both).next().unwrap().unwrap();
        assert_eq!(rec.reparse_tag, TAG_SYMLINK);
        assert_eq!(rec.file_id, Some(5));
        // A file without the reparse attribute reports no tag.
        let buf = chain(vec![build(Layout::Both, "f", 0x20, 1234, 0, 1, 1, 0, 0)]);
        let rec = records(&buf, Layout::Both).next().unwrap().unwrap();
        assert_eq!((rec.reparse_tag, rec.file_id), (0, None));
    }

    #[test]
    fn damaged_buffers_are_errors() {
        let good = chain(vec![
            build(Layout::Extended, "one", 0, 0, 1, 1, 1, 0, 0),
            build(Layout::Extended, "two", 0, 0, 2, 1, 1, 0, 0),
        ]);
        // Short.
        assert!(
            records(&good[..40], Layout::Extended)
                .next()
                .unwrap()
                .is_err()
        );
        // The first record's next-offset runs past the buffer.
        let mut bad = good.clone();
        bad[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            records(&bad, Layout::Extended).next().unwrap(),
            Err(ListError::BadOffset)
        );
        // Odd name length.
        let mut bad = good.clone();
        bad[60..64].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(
            records(&bad, Layout::Extended).next().unwrap(),
            Err(ListError::BadName)
        );
        // Name longer than the record.
        let mut bad = good.clone();
        bad[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            records(&bad, Layout::Extended).next().unwrap(),
            Err(ListError::BadName)
        );
        // Next offset inside the name.
        let mut bad = good;
        bad[0..4].copy_from_slice(&90u32.to_le_bytes());
        assert_eq!(
            records(&bad, Layout::Extended).next().unwrap(),
            Err(ListError::BadOffset)
        );
    }

    #[test]
    fn negative_sizes_do_not_wrap() {
        let buf = chain(vec![build(
            Layout::Extended,
            "x",
            0,
            0,
            1,
            -5,
            -9,
            i64::MIN,
            0,
        )]);
        let rec = records(&buf, Layout::Extended).next().unwrap().unwrap();
        assert_eq!((rec.logical, rec.allocated), (0, 0));
        let _ = rec.mtime; // saturating, no panic
    }

    #[test]
    fn classification_follows_the_rules() {
        let plain = classify(0x20, 0);
        assert_eq!(plain.class, EntryClass::File);
        assert!(!plain.placeholder && !plain.not_followed);

        let dir = classify(attr::DIRECTORY, 0);
        assert_eq!((dir.class, dir.descend), (EntryClass::Dir, true));

        // Junction and symlink directories are never entered.
        let junction = classify(attr::DIRECTORY | attr::REPARSE_POINT, 0xA000_0003);
        assert_eq!(junction.class, EntryClass::Other);
        assert!(!junction.descend && junction.not_followed);
        let symlink = classify(attr::DIRECTORY | attr::REPARSE_POINT, TAG_SYMLINK);
        assert_eq!(symlink.class, EntryClass::Symlink);
        assert!(!symlink.descend);

        // Cloud placeholders, by attribute or by tag, are never opened.
        for attrs in [
            attr::RECALL_ON_DATA_ACCESS,
            attr::RECALL_ON_OPEN,
            attr::OFFLINE,
        ] {
            let c = classify(attrs, 0);
            assert!(c.placeholder, "{attrs:#x}");
            let c = classify(attrs | attr::DIRECTORY, 0);
            assert!(c.placeholder && !c.descend, "{attrs:#x} dir");
        }
        let cloud_dir = classify(attr::DIRECTORY | attr::REPARSE_POINT, 0x9000_001A);
        assert!(cloud_dir.placeholder && !cloud_dir.descend);
        assert_eq!(cloud_dir.class, EntryClass::Dir);
        let cloud_file = classify(attr::REPARSE_POINT, 0x9000_F01A);
        assert!(cloud_file.placeholder);
        assert_eq!(cloud_file.class, EntryClass::File);
        // Other reparse files are counted but flagged as not followed.
        let dedup = classify(attr::REPARSE_POINT, 0x8000_0013);
        assert!(dedup.not_followed && !dedup.placeholder);
        assert_eq!(dedup.class, EntryClass::File);
    }

    #[test]
    fn only_estimated_listings_set_the_estimate_flag() {
        use bs_tree::NodeFlags;
        let plain = classify(0x20, 0);
        assert_eq!(entry_flags(&plain, false), NodeFlags::EMPTY);
        assert_eq!(entry_flags(&plain, true), NodeFlags::ESTIMATED_SIZE);
        // It sits beside the other producer flags without replacing them.
        let link = classify(attr::DIRECTORY | attr::REPARSE_POINT, 0xA000_0003);
        let both = entry_flags(&link, true);
        assert!(both.contains(NodeFlags::NOT_FOLLOWED));
        assert!(both.contains(NodeFlags::ESTIMATED_SIZE));
        let cloud = classify(attr::OFFLINE, 0);
        let flags = entry_flags(&cloud, false);
        assert!(flags.contains(NodeFlags::CLOUD_PLACEHOLDER));
        assert!(!flags.contains(NodeFlags::ESTIMATED_SIZE));
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(filetime_to_unix(EPOCH_GAP_SECS * 10_000_000), 0);
        assert_eq!(
            filetime_to_unix((EPOCH_GAP_SECS + 86_400) * 10_000_000 + 9_999_999),
            86_400
        );
        assert!(filetime_to_unix(i64::MIN) < 0);
        // Before 1970.
        assert_eq!(filetime_to_unix(0), -EPOCH_GAP_SECS);
    }

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn joining_never_doubles_the_separator() {
        // A volume root already ends in a separator.
        assert_eq!(join_path(&w(r"\\?\C:\"), &w("Users")), w(r"\\?\C:\Users"));
        assert_eq!(join_path(&w(r"C:\"), &w("*")), w(r"C:\*"));
        // A directory does not.
        assert_eq!(
            join_path(&w(r"\\?\C:\Users"), &w("me")),
            w(r"\\?\C:\Users\me")
        );
        assert_eq!(
            join_path(&w(r"\\?\C:\Users"), &w("*")),
            w(r"\\?\C:\Users\*")
        );
        // A share.
        assert_eq!(
            join_path(&w(r"\\?\UNC\srv\share"), &w("a")),
            w(r"\\?\UNC\srv\share\a")
        );
        // Nothing to join to.
        assert_eq!(join_path(&[], &w("a")), w("a"));
    }

    #[test]
    fn only_single_components_may_be_opened_by_name() {
        assert!(is_plain_component(&w("plain name.txt")));
        assert!(is_plain_component(&w("caf\u{e9}")));
        // An unpaired surrogate is a legal name.
        assert!(is_plain_component(&[u16::from(b'a'), 0xD800]));
        assert!(is_plain_component(&w("..a")));
        assert!(!is_plain_component(&[]));
        assert!(!is_plain_component(&w(r"a\b")));
        assert!(!is_plain_component(&w("a/b")));
        assert!(!is_plain_component(&w(r"..\..\Windows")));
        assert!(!is_plain_component(&[u16::from(b'a'), 0, u16::from(b'b')]));
    }

    fn utf8(units: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        wtf8_from_utf16(units.iter().copied(), &mut out);
        out
    }

    #[test]
    fn wtf8_matches_utf8_for_valid_text_and_keeps_lone_surrogates() {
        for s in [
            "",
            "plain",
            "caf\u{e9}",
            "\u{65e5}\u{672c}",
            "emoji \u{1F600} end",
        ] {
            let units: Vec<u16> = s.encode_utf16().collect();
            assert_eq!(utf8(&units), s.as_bytes(), "{s}");
        }
        // A lone high surrogate and a lone low surrogate: three bytes each.
        assert_eq!(utf8(&[0xD800]), [0xED, 0xA0, 0x80]);
        assert_eq!(utf8(&[0xDFFF, b'a' as u16]), [0xED, 0xBF, 0xBF, b'a']);
        // A high surrogate followed by a non-surrogate stays separate.
        assert_eq!(utf8(&[0xD83D, b'x' as u16]), [0xED, 0xA0, 0xBD, b'x']);
    }

    #[test]
    fn wtf8_decodes_back_to_the_units_it_came_from() {
        for s in [
            "",
            "plain",
            "caf\u{e9}",
            "\u{65e5}\u{672c}",
            "emoji \u{1F600} end",
        ] {
            let units: Vec<u16> = s.encode_utf16().collect();
            assert_eq!(utf16_from_wtf8(&utf8(&units)), units, "{s}");
        }
        for units in [
            vec![0xD800u16],
            vec![0xDFFF, 0x61],
            vec![0xD83D, 0x78],
            vec![0xDC00, 0xD800],
        ] {
            assert_eq!(utf16_from_wtf8(&utf8(&units)), units);
        }
        // Not WTF-8: each bad byte stands for one replacement character.
        assert_eq!(utf16_from_wtf8(&[0xFF, 0x61]), [0xFFFD, 0x61]);
        assert_eq!(utf16_from_wtf8(&[0xE2, 0x82]), [0xFFFD, 0xFFFD]);
        assert_eq!(utf16_from_wtf8(&[0xC0, 0x80]), [0xFFFD, 0xFFFD]);
        assert_eq!(utf16_from_wtf8(&[0xF4, 0x90, 0x80, 0x80]).len(), 4);
    }

    proptest! {
        #[test]
        fn any_buffer_is_handled_without_panic(buf in proptest::collection::vec(any::<u8>(), 0..400), extended in any::<bool>()) {
            let layout = if extended { Layout::Extended } else { Layout::Both };
            let mut n = 0;
            for item in records(&buf, layout) {
                n += 1;
                prop_assert!(n <= buf.len() + 1);
                if let Ok(r) = item {
                    // The name range is inside the buffer.
                    prop_assert!(r.name_at + r.name_len <= buf.len());
                    let _ = name_units(&buf, &r).count();
                }
            }
        }

        #[test]
        fn utf16_survives_the_round_trip_and_any_bytes_decode(units in proptest::collection::vec(any::<u16>(), 0..60), junk in proptest::collection::vec(any::<u8>(), 0..60)) {
            prop_assert_eq!(utf16_from_wtf8(&utf8(&units)), units);
            let _ = utf16_from_wtf8(&junk);
        }

        #[test]
        fn wtf8_is_lossless_and_never_longer_than_three_bytes_per_unit(units in proptest::collection::vec(any::<u16>(), 0..60)) {
            let out = utf8(&units);
            prop_assert!(out.len() <= units.len() * 3);
            // Decode it back by hand: generalized UTF-8.
            let mut back = Vec::new();
            let mut i = 0;
            while i < out.len() {
                let b = out[i];
                let (code, n) = if b < 0x80 { (u32::from(b), 1) }
                    else if b < 0xE0 { ((u32::from(b & 0x1F) << 6) | u32::from(out[i+1] & 0x3F), 2) }
                    else if b < 0xF0 { ((u32::from(b & 0x0F) << 12) | (u32::from(out[i+1] & 0x3F) << 6) | u32::from(out[i+2] & 0x3F), 3) }
                    else { ((u32::from(b & 0x07) << 18) | (u32::from(out[i+1] & 0x3F) << 12) | (u32::from(out[i+2] & 0x3F) << 6) | u32::from(out[i+3] & 0x3F), 4) };
                i += n;
                if code >= 0x1_0000 {
                    let c = code - 0x1_0000;
                    back.push(0xD800 + (c >> 10) as u16);
                    back.push(0xDC00 + (c & 0x3FF) as u16);
                } else {
                    back.push(code as u16);
                }
            }
            prop_assert_eq!(back, units);
        }
    }
}
