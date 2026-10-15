//! The items the reader looks at, decoded with every offset and length
//! checked. The layouts are those of the kernel's `btrfs_tree.h`.

use crate::le::{bytes_at, u8_at, u16_at, u32_at, u64_at};

pub const INODE_ITEM: u8 = 1;
pub const DIR_INDEX: u8 = 96;
pub const EXTENT_DATA: u8 = 108;
pub const ROOT_ITEM: u8 = 132;
/// A subvolume's place in its parent: keyed (parent tree, this, child tree).
pub const ROOT_REF: u8 = 156;

pub const ROOT_TREE_ID: u64 = 1;
pub const FS_TREE_ID: u64 = 5;
/// The first object id that is an inode (or a subvolume's tree) and not
/// bookkeeping; every subvolume's root directory is this inode.
pub const FIRST_FREE: u64 = 256;
/// The last such id. Above it lie the orphan and balance bookkeeping items.
pub const LAST_FREE: u64 = u64::MAX - 255;

const INODE_ITEM_LEN: usize = 160;
const DIR_ITEM_HEADER: usize = 30;
const EXTENT_HEADER: usize = 21;
const EXTENT_REGULAR_LEN: usize = 53;

pub(crate) const S_IFMT: u32 = 0o170_000;
pub(crate) const S_IFREG: u32 = 0o100_000;
pub(crate) const S_IFDIR: u32 = 0o040_000;
pub(crate) const S_IFLNK: u32 = 0o120_000;

/// The fields of an inode item that the reader uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InodeItem {
    pub size: u64,
    pub nlink: u32,
    pub mode: u32,
    pub mtime: i64,
}

pub(crate) fn parse_inode(data: &[u8]) -> Option<InodeItem> {
    if data.len() < INODE_ITEM_LEN {
        return None;
    }
    // Offsets: size 16, nlink 40, mode 52, atime 112, ctime 124, mtime 136.
    let sec = u64_at(data, 136)?;
    Some(InodeItem {
        size: u64_at(data, 16)?,
        nlink: u32_at(data, 40)?,
        mode: u32_at(data, 52)?,
        // Seconds are signed on disk; keep the bits.
        mtime: i64::from_le_bytes(sec.to_le_bytes()),
    })
}

/// What a directory entry points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// An inode of the same tree.
    Inode(u64),
    /// The root directory of another subvolume, by its tree id.
    Subvolume(u64),
}

/// One name in a directory item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirEntry<'a> {
    pub target: Target,
    /// The file type (`BTRFS_FT_*`).
    pub ty: u8,
    pub name: &'a [u8],
}

/// The names in a directory item, which holds one for an index item and
/// several, packed one after the other, for a hash item. `Err` once an entry
/// does not fit; the entries before it have been yielded.
pub(crate) struct DirEntries<'a> {
    data: &'a [u8],
    pos: usize,
    failed: bool,
}

pub(crate) fn dir_entries(data: &[u8]) -> DirEntries<'_> {
    DirEntries {
        data,
        pos: 0,
        failed: false,
    }
}

impl<'a> Iterator for DirEntries<'a> {
    type Item = Result<DirEntry<'a>, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.data.len() {
            return None;
        }
        let parsed = self.parse_one();
        if parsed.is_err() {
            self.failed = true;
        }
        Some(parsed)
    }
}

impl<'a> DirEntries<'a> {
    fn parse_one(&mut self) -> Result<DirEntry<'a>, ()> {
        let rest = self.data.get(self.pos..).ok_or(())?;
        let objectid = u64_at(rest, 0).ok_or(())?;
        let key_type = u8_at(rest, 8).ok_or(())?;
        let data_len = usize::from(u16_at(rest, 25).ok_or(())?);
        let name_len = usize::from(u16_at(rest, 27).ok_or(())?);
        let ty = u8_at(rest, 29).ok_or(())?;
        let name_at = DIR_ITEM_HEADER;
        let name = bytes_at(rest, name_at, name_len).ok_or(())?;
        let total = name_at
            .checked_add(name_len)
            .and_then(|n| n.checked_add(data_len))
            .filter(|&t| t <= rest.len())
            .ok_or(())?;
        let target = match key_type {
            INODE_ITEM => Target::Inode(objectid),
            ROOT_ITEM => Target::Subvolume(objectid),
            // An extended attribute, or a key of a type nothing lists.
            _ => return Err(()),
        };
        self.pos = self.pos.checked_add(total).ok_or(())?;
        Ok(DirEntry { target, ty, name })
    }
}

/// A root reference item: where the kernel says a subvolume is nested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RootRef<'a> {
    /// The directory of the parent subvolume that holds the entry.
    pub dirid: u64,
    /// The name of the entry in that directory.
    pub name: &'a [u8],
}

/// Decodes a root reference item (`struct btrfs_root_ref`: directory id,
/// sequence, name length, then the name).
///
/// The kernel's tree checker takes only an item that is exactly the header
/// and a name of 1 to 255 bytes; anything else is damage, and so it is here.
pub(crate) fn parse_root_ref(data: &[u8]) -> Option<RootRef<'_>> {
    let dirid = u64_at(data, 0)?;
    let name_len = usize::from(u16_at(data, 16)?);
    if name_len == 0 || name_len > 255 || data.len() != 18usize.checked_add(name_len)? {
        return None;
    }
    let name = bytes_at(data, 18, name_len)?;
    Some(RootRef { dirid, name })
}

/// A file extent item, reduced to what the accounting needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Extent {
    /// Data kept in the tree itself, as many bytes as stored (compressed, if
    /// it is): it costs no extent, only room in the tree's blocks.
    Inline { bytes: u64 },
    /// A range of disk blocks, or a hole when `disk_bytenr` is zero.
    Disk {
        disk_bytenr: u64,
        disk_num_bytes: u64,
        compressed: bool,
        /// Reserved with `fallocate` and not written yet.
        prealloc: bool,
    },
}

pub(crate) fn parse_extent(data: &[u8]) -> Option<Extent> {
    if data.len() < EXTENT_HEADER {
        return None;
    }
    let compression = u8_at(data, 16)?;
    let ty = u8_at(data, 20)?;
    match ty {
        0 => Some(Extent::Inline {
            bytes: data.len().saturating_sub(EXTENT_HEADER) as u64,
        }),
        1 | 2 => {
            if data.len() < EXTENT_REGULAR_LEN {
                return None;
            }
            Some(Extent::Disk {
                disk_bytenr: u64_at(data, 21)?,
                disk_num_bytes: u64_at(data, 29)?,
                compressed: compression != 0,
                prealloc: ty == 2,
            })
        }
        _ => None,
    }
}

/// For a file extent item that does not parse: the disk address it still
/// readably names, if it is cut short of a regular extent but long enough to
/// say what it refers to. `Some(0)` is a hole. `None` when nothing can be
/// told (too short to know its type, or a type that does not exist), which
/// the caller must treat as a reference to anything.
pub(crate) fn damaged_extent_target(data: &[u8]) -> Option<u64> {
    match u8_at(data, 20)? {
        1 | 2 => u64_at(data, 21),
        _ => None,
    }
}

/// The tree id a `ROOT_ITEM` key names, if it names a subvolume: the file
/// system tree itself or an id in the inode range.
pub(crate) fn is_subvolume_id(id: u64) -> bool {
    id == FS_TREE_ID || (FIRST_FREE..=LAST_FREE).contains(&id)
}

/// The kind of file an inode mode says.
pub(crate) fn mode_kind(mode: u32) -> Option<ModeKind> {
    match mode & S_IFMT {
        S_IFREG => Some(ModeKind::Regular),
        S_IFDIR => Some(ModeKind::Directory),
        S_IFLNK => Some(ModeKind::Symlink),
        0o020_000 | 0o060_000 | 0o010_000 | 0o140_000 => Some(ModeKind::Special),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModeKind {
    Regular,
    Directory,
    Symlink,
    Special,
}

/// Whether a directory entry's recorded type (`BTRFS_FT_*`) agrees with the
/// inode it points at. An unknown recorded type agrees with anything.
pub(crate) fn type_agrees(ft: u8, kind: ModeKind, mode: u32) -> bool {
    match ft {
        0 => true,
        1 => kind == ModeKind::Regular,
        2 => kind == ModeKind::Directory,
        7 => kind == ModeKind::Symlink,
        3 => mode & S_IFMT == 0o020_000,
        4 => mode & S_IFMT == 0o060_000,
        5 => mode & S_IFMT == 0o010_000,
        6 => mode & S_IFMT == 0o140_000,
        _ => false,
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

    fn inode_bytes(size: u64, nlink: u32, mode: u32, mtime: u64) -> Vec<u8> {
        let mut b = vec![0u8; 160];
        b[16..24].copy_from_slice(&size.to_le_bytes());
        b[40..44].copy_from_slice(&nlink.to_le_bytes());
        b[52..56].copy_from_slice(&mode.to_le_bytes());
        b[136..144].copy_from_slice(&mtime.to_le_bytes());
        b
    }

    fn dir_item(target: u64, key_type: u8, ty: u8, name: &[u8], extra: usize) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&target.to_le_bytes());
        b.push(key_type);
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&1u64.to_le_bytes());
        b.extend_from_slice(&(extra as u16).to_le_bytes());
        b.extend_from_slice(&(name.len() as u16).to_le_bytes());
        b.push(ty);
        b.extend_from_slice(name);
        b.extend(std::iter::repeat_n(0xAA, extra));
        b
    }

    #[test]
    fn an_inode_item_gives_size_links_mode_and_time() {
        let i = parse_inode(&inode_bytes(1234, 3, S_IFREG | 0o644, 1_700_000_000)).unwrap();
        assert_eq!(
            (i.size, i.nlink, i.mode, i.mtime),
            (1234, 3, S_IFREG | 0o644, 1_700_000_000)
        );
        // Negative seconds keep their sign.
        let neg = parse_inode(&inode_bytes(0, 1, S_IFDIR, (-5i64) as u64)).unwrap();
        assert_eq!(neg.mtime, -5);
        // Anything shorter than an inode item is refused.
        for cut in 0..160 {
            assert!(
                parse_inode(&inode_bytes(0, 0, 0, 0)[..cut]).is_none(),
                "{cut}"
            );
        }
    }

    #[test]
    fn directory_entries_are_read_one_after_the_other() {
        let mut item = dir_item(300, INODE_ITEM, 1, b"file", 0);
        item.extend(dir_item(400, ROOT_ITEM, 2, b"sub", 4));
        let got: Vec<_> = dir_entries(&item).collect();
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0],
            Ok(DirEntry {
                target: Target::Inode(300),
                ty: 1,
                name: b"file"
            })
        );
        assert_eq!(
            got[1],
            Ok(DirEntry {
                target: Target::Subvolume(400),
                ty: 2,
                name: b"sub"
            })
        );
    }

    #[test]
    fn a_damaged_entry_ends_the_item_with_one_error() {
        let good = dir_item(300, INODE_ITEM, 1, b"file", 0);
        for cut in 1..good.len() {
            let got: Vec<_> = dir_entries(&good[..cut]).collect();
            assert_eq!(got, vec![Err(())], "cut {cut}");
        }
        // A name longer than the item.
        let mut long = good.clone();
        long[27..29].copy_from_slice(&500u16.to_le_bytes());
        assert_eq!(dir_entries(&long).collect::<Vec<_>>(), vec![Err(())]);
        // Data longer than the item.
        let mut data = good.clone();
        data[25..27].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(dir_entries(&data).collect::<Vec<_>>(), vec![Err(())]);
        // An entry that points at a key of another kind (an xattr).
        let other = dir_item(300, 24, 1, b"user.x", 0);
        assert_eq!(dir_entries(&other).collect::<Vec<_>>(), vec![Err(())]);
        // The entry before a damaged one is still delivered.
        let mut two = good;
        two.extend_from_slice(&[1, 2, 3]);
        let got: Vec<_> = dir_entries(&two).collect();
        assert_eq!(got.len(), 2);
        assert!(got[0].is_ok() && got[1].is_err());
    }

    #[test]
    fn extent_items_distinguish_inline_regular_hole_and_compressed() {
        let mut inline = vec![0u8; 21 + 10];
        inline[20] = 0;
        assert_eq!(parse_extent(&inline), Some(Extent::Inline { bytes: 10 }));
        let mut regular = vec![0u8; 53];
        regular[20] = 1;
        regular[21..29].copy_from_slice(&4096u64.to_le_bytes());
        regular[29..37].copy_from_slice(&8192u64.to_le_bytes());
        assert_eq!(
            parse_extent(&regular),
            Some(Extent::Disk {
                disk_bytenr: 4096,
                disk_num_bytes: 8192,
                compressed: false,
                prealloc: false
            })
        );
        regular[16] = 3;
        regular[20] = 2;
        assert!(matches!(
            parse_extent(&regular),
            Some(Extent::Disk {
                compressed: true,
                prealloc: true,
                ..
            })
        ));
        // A hole has no disk address.
        regular[21..29].copy_from_slice(&0u64.to_le_bytes());
        assert!(matches!(
            parse_extent(&regular),
            Some(Extent::Disk { disk_bytenr: 0, .. })
        ));
        // Too short for its type, or of a type that does not exist.
        assert_eq!(parse_extent(&regular[..52]), None);
        assert_eq!(parse_extent(&regular[..20]), None);
        regular[20] = 9;
        assert_eq!(parse_extent(&regular), None);
    }

    #[test]
    fn modes_and_recorded_types_are_compared() {
        assert_eq!(mode_kind(S_IFREG | 0o600), Some(ModeKind::Regular));
        assert_eq!(mode_kind(0), None);
        assert_eq!(mode_kind(0o030_000), None);
        assert!(type_agrees(1, ModeKind::Regular, S_IFREG));
        assert!(!type_agrees(1, ModeKind::Directory, S_IFDIR));
        assert!(type_agrees(0, ModeKind::Directory, S_IFDIR));
        assert!(type_agrees(6, ModeKind::Special, 0o140_000));
        assert!(!type_agrees(6, ModeKind::Special, 0o010_000));
        assert!(!type_agrees(200, ModeKind::Regular, S_IFREG));
    }

    #[test]
    fn a_cut_short_extent_item_still_names_its_disk_address_when_it_can() {
        let mut item = vec![0u8; 53];
        item[20] = 1;
        item[21..29].copy_from_slice(&0x1234_5000u64.to_le_bytes());
        assert_eq!(parse_extent(&item[..40]), None);
        assert_eq!(damaged_extent_target(&item[..40]), Some(0x1234_5000));
        assert_eq!(damaged_extent_target(&item[..29]), Some(0x1234_5000));
        // Too short to hold the address, or to know the type.
        assert_eq!(damaged_extent_target(&item[..28]), None);
        assert_eq!(damaged_extent_target(&item[..20]), None);
        // An inline item holds data where the address would be; a type that
        // does not exist says nothing.
        item[20] = 0;
        assert_eq!(damaged_extent_target(&item), None);
        item[20] = 9;
        assert_eq!(damaged_extent_target(&item), None);
    }

    fn root_ref_bytes(dirid: u64, name: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&dirid.to_le_bytes());
        b.extend_from_slice(&7u64.to_le_bytes());
        b.extend_from_slice(&(name.len() as u16).to_le_bytes());
        b.extend_from_slice(name);
        b
    }

    #[test]
    fn a_root_reference_gives_its_directory_and_name() {
        let item = root_ref_bytes(260, b"inner");
        assert_eq!(
            parse_root_ref(&item),
            Some(RootRef {
                dirid: 260,
                name: b"inner"
            })
        );
        // Anything cut short of its name, or of its header, is refused.
        for cut in 0..item.len() {
            assert_eq!(parse_root_ref(&item[..cut]), None, "{cut}");
        }
        // A name length that runs past the item.
        let mut long = item.clone();
        long[16..18].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(parse_root_ref(&long), None);
        // Bytes after the name, an empty name and a name longer than a
        // name can be are damage, as the kernel's tree checker has it.
        let mut extra = item;
        extra.extend_from_slice(b"junk");
        assert_eq!(parse_root_ref(&extra), None);
        assert_eq!(parse_root_ref(&root_ref_bytes(260, b"")), None);
        assert!(parse_root_ref(&root_ref_bytes(260, &[b'a'; 255])).is_some());
        assert_eq!(parse_root_ref(&root_ref_bytes(260, &[b'a'; 256])), None);
    }

    #[test]
    fn subvolume_ids_are_the_inode_range_and_the_default_tree() {
        assert!(is_subvolume_id(5));
        assert!(is_subvolume_id(256));
        assert!(is_subvolume_id(LAST_FREE));
        for id in [0, 1, 2, 3, 4, 6, 255, LAST_FREE + 1, u64::MAX] {
            assert!(!is_subvolume_id(id), "{id}");
        }
    }
}
