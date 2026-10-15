//! The superblock, checked until every number derived from it is safe to use
//! as a size, an offset or a count.

use crate::error::Ext4Error;
use crate::le::{bytes_at, join64, u16_at, u32_at};
use crate::source::ByteSource;

/// The superblock sits 1024 bytes into the device whatever the block size.
const SUPERBLOCK_OFFSET: u64 = 1024;
const SUPERBLOCK_LEN: usize = 1024;
const MAGIC: u16 = 0xEF53;

// Compatible features.
const COMPAT_SPARSE_SUPER2: u32 = 0x0200;

// Incompatible features: a reader must understand every one that is set.
const INCOMPAT_COMPRESSION: u32 = 0x0001;
const INCOMPAT_FILETYPE: u32 = 0x0002;
const INCOMPAT_RECOVER: u32 = 0x0004;
const INCOMPAT_JOURNAL_DEV: u32 = 0x0008;
const INCOMPAT_META_BG: u32 = 0x0010;
const INCOMPAT_EXTENTS: u32 = 0x0040;
const INCOMPAT_64BIT: u32 = 0x0080;
const INCOMPAT_MMP: u32 = 0x0100;
const INCOMPAT_FLEX_BG: u32 = 0x0200;
const INCOMPAT_EA_INODE: u32 = 0x0400;
const INCOMPAT_DIRDATA: u32 = 0x1000;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_LARGEDIR: u32 = 0x4000;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const INCOMPAT_ENCRYPT: u32 = 0x10000;
const INCOMPAT_CASEFOLD: u32 = 0x20000;
/// Everything above that a reader can cope with. Compression, an external
/// journal device and per-entry directory data are not among them.
const INCOMPAT_KNOWN: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_META_BG
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_MMP
    | INCOMPAT_FLEX_BG
    | INCOMPAT_EA_INODE
    | INCOMPAT_CSUM_SEED
    | INCOMPAT_LARGEDIR
    | INCOMPAT_INLINE_DATA
    | INCOMPAT_ENCRYPT
    | INCOMPAT_CASEFOLD;

// Read-only compatible features.
const RO_SPARSE_SUPER: u32 = 0x0001;
const RO_HUGE_FILE: u32 = 0x0008;
const RO_GDT_CSUM: u32 = 0x0010;
const RO_BIGALLOC: u32 = 0x0200;
const RO_METADATA_CSUM: u32 = 0x0400;

/// The largest block size ext4 allows.
const MAX_LOG_BLOCK_SIZE: u32 = 6;

/// Reserved inodes below this number are never ordinary files.
pub(crate) const ROOT_INODE: u32 = 2;

/// The numbers that describe a file system's layout, all checked.
#[derive(Clone, Debug)]
pub(crate) struct Geometry {
    pub block_size: u32,
    pub blocks_count: u64,
    pub first_data_block: u64,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u32,
    pub groups: u32,
    pub inodes_count: u32,
    pub first_ino: u32,
    pub desc_size: u32,
    pub first_meta_bg: u32,
    pub backup_bgs: [u32; 2],
    pub incompat: u32,
    pub compat: u32,
    pub ro_compat: u32,
    pub uuid: [u8; 16],
}

impl Geometry {
    pub fn has_filetype(&self) -> bool {
        self.incompat & INCOMPAT_FILETYPE != 0
    }

    pub fn has_meta_bg(&self) -> bool {
        self.incompat & INCOMPAT_META_BG != 0
    }

    pub fn has_large_dir(&self) -> bool {
        self.incompat & INCOMPAT_LARGEDIR != 0
    }

    pub fn needs_recovery(&self) -> bool {
        self.incompat & INCOMPAT_RECOVER != 0
    }

    pub fn has_huge_file(&self) -> bool {
        self.ro_compat & RO_HUGE_FILE != 0
    }

    pub fn has_metadata_csum(&self) -> bool {
        self.ro_compat & RO_METADATA_CSUM != 0
    }

    /// Whether group descriptors carry the uninitialized flags and the count
    /// of unused inodes. Without either checksum feature they are not
    /// maintained and mean nothing.
    pub fn has_group_flags(&self) -> bool {
        self.ro_compat & (RO_GDT_CSUM | RO_METADATA_CSUM) != 0
    }

    pub fn is_64bit(&self) -> bool {
        self.incompat & INCOMPAT_64BIT != 0
    }

    /// Blocks taken by one group's inode table.
    pub fn inode_table_blocks(&self) -> u64 {
        let bytes = u64::from(self.inodes_per_group).saturating_mul(u64::from(self.inode_size));
        bytes.div_ceil(u64::from(self.block_size))
    }

    /// Whether a block group carries a superblock backup (and a copy of the
    /// group descriptors).
    pub fn group_has_super(&self, group: u32) -> bool {
        if group == 0 {
            return true;
        }
        if self.compat & COMPAT_SPARSE_SUPER2 != 0 {
            return self.backup_bgs.contains(&group);
        }
        if self.ro_compat & RO_SPARSE_SUPER == 0 {
            return true;
        }
        if group <= 1 {
            return true;
        }
        [3u32, 5, 7].iter().any(|&base| is_power_of(group, base))
    }
}

fn is_power_of(n: u32, base: u32) -> bool {
    if n == 0 || base < 2 {
        return false;
    }
    let mut power = 1u64;
    while power < u64::from(n) {
        power = power.saturating_mul(u64::from(base));
    }
    power == u64::from(n)
}

/// Reads and checks the superblock.
pub(crate) fn read_geometry<S: ByteSource + ?Sized>(src: &S) -> Result<Geometry, Ext4Error> {
    let mut raw = vec![0u8; SUPERBLOCK_LEN];
    src.read_at(SUPERBLOCK_OFFSET, &mut raw)?;
    parse(&raw, src.len())
}

fn parse(sb: &[u8], device_len: u64) -> Result<Geometry, Ext4Error> {
    const SHORT: Ext4Error = Ext4Error::BadSuperblock("short superblock");
    let short = || SHORT;
    if u16_at(sb, 0x38).ok_or_else(short)? != MAGIC {
        return Err(Ext4Error::NotExt4);
    }
    let rev_level = u32_at(sb, 0x4C).ok_or_else(short)?;
    let (compat, incompat, ro_compat) = if rev_level >= 1 {
        (
            u32_at(sb, 0x5C).ok_or_else(short)?,
            u32_at(sb, 0x60).ok_or_else(short)?,
            u32_at(sb, 0x64).ok_or_else(short)?,
        )
    } else {
        (0, 0, 0)
    };
    if incompat & INCOMPAT_JOURNAL_DEV != 0 {
        return Err(Ext4Error::Unsupported("external journal device"));
    }
    if incompat & INCOMPAT_COMPRESSION != 0 {
        return Err(Ext4Error::Unsupported("compression"));
    }
    if incompat & INCOMPAT_DIRDATA != 0 {
        return Err(Ext4Error::Unsupported("directory entry data"));
    }
    if incompat & !INCOMPAT_KNOWN != 0 {
        return Err(Ext4Error::Unsupported("unknown incompatible feature"));
    }

    let log_block_size = u32_at(sb, 0x18).ok_or_else(short)?;
    if log_block_size > MAX_LOG_BLOCK_SIZE {
        return Err(Ext4Error::BadSuperblock("block size out of range"));
    }
    let block_size = 1024u32
        .checked_shl(log_block_size)
        .ok_or(Ext4Error::BadSuperblock("block size out of range"))?;

    let blocks_lo = u32_at(sb, 0x04).ok_or_else(short)?;
    let is_64bit = incompat & INCOMPAT_64BIT != 0;
    let blocks_hi = if is_64bit {
        u32_at(sb, 0x150).ok_or_else(short)?
    } else {
        0
    };
    let blocks_count = join64(blocks_lo, blocks_hi);
    if blocks_count == 0 {
        return Err(Ext4Error::BadSuperblock("no blocks"));
    }
    let device_blocks = device_len.checked_div(u64::from(block_size)).unwrap_or(0);
    if blocks_count > device_blocks {
        return Err(Ext4Error::BadSuperblock(
            "the file system is larger than its device",
        ));
    }
    let first_data_block = u64::from(u32_at(sb, 0x14).ok_or_else(short)?);
    if first_data_block >= blocks_count {
        return Err(Ext4Error::BadSuperblock("first data block out of range"));
    }

    let blocks_per_group = u32_at(sb, 0x20).ok_or_else(short)?;
    let inodes_per_group = u32_at(sb, 0x28).ok_or_else(short)?;
    let bits_per_block = block_size.saturating_mul(8);
    // With big allocation clusters, a group's bitmap has a bit per cluster, so
    // it is the cluster count that must fit in a block of bits.
    let bitmap_bits = if ro_compat & RO_BIGALLOC != 0 {
        u32_at(sb, 0x24).ok_or_else(short)?
    } else {
        blocks_per_group
    };
    if blocks_per_group == 0 || bitmap_bits == 0 || bitmap_bits > bits_per_block {
        return Err(Ext4Error::BadSuperblock("blocks per group out of range"));
    }
    if inodes_per_group == 0 || inodes_per_group > bits_per_block {
        return Err(Ext4Error::BadSuperblock("inodes per group out of range"));
    }

    let inode_size = if rev_level >= 1 {
        u32::from(u16_at(sb, 0x58).ok_or_else(short)?)
    } else {
        128
    };
    if !(128..=block_size).contains(&inode_size) || !inode_size.is_power_of_two() {
        return Err(Ext4Error::BadSuperblock("inode size out of range"));
    }
    let first_ino = if rev_level >= 1 {
        u32_at(sb, 0x54).ok_or_else(short)?
    } else {
        11
    };
    if first_ino < 11 || first_ino > inodes_per_group {
        return Err(Ext4Error::BadSuperblock("first inode out of range"));
    }

    let desc_size = if is_64bit {
        let size = u32::from(u16_at(sb, 0xFE).ok_or_else(short)?);
        if !(64..=1024).contains(&size) || !size.is_power_of_two() || size > block_size {
            return Err(Ext4Error::BadSuperblock(
                "group descriptor size out of range",
            ));
        }
        size
    } else {
        32
    };

    let data_blocks = blocks_count.saturating_sub(first_data_block);
    let groups_wide = data_blocks.div_ceil(u64::from(blocks_per_group));
    let groups = u32::try_from(groups_wide)
        .map_err(|_| Ext4Error::BadSuperblock("too many block groups"))?;
    let inodes_count = u32_at(sb, 0x00).ok_or_else(short)?;
    let expected_inodes = u64::from(groups).saturating_mul(u64::from(inodes_per_group));
    if expected_inodes != u64::from(inodes_count) {
        return Err(Ext4Error::BadSuperblock(
            "inode count does not match the groups",
        ));
    }
    // The inode tables have to fit in the file system.
    let table_bytes = expected_inodes.saturating_mul(u64::from(inode_size));
    if table_bytes > blocks_count.saturating_mul(u64::from(block_size)) {
        return Err(Ext4Error::BadSuperblock(
            "inode tables larger than the file system",
        ));
    }
    // The descriptor table has to fit as well.
    let desc_bytes = u64::from(groups).saturating_mul(u64::from(desc_size));
    if desc_bytes.div_ceil(u64::from(block_size)) > blocks_count {
        return Err(Ext4Error::BadSuperblock(
            "descriptor table larger than the file system",
        ));
    }

    let uuid: [u8; 16] = bytes_at(sb, 0x68, 16)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(short)?;
    let first_meta_bg = u32_at(sb, 0x104).ok_or_else(short)?;
    let backup_bgs = [
        u32_at(sb, 0x24C).ok_or_else(short)?,
        u32_at(sb, 0x250).ok_or_else(short)?,
    ];

    Ok(Geometry {
        block_size,
        blocks_count,
        first_data_block,
        blocks_per_group,
        inodes_per_group,
        inode_size,
        groups,
        inodes_count,
        first_ino,
        desc_size,
        first_meta_bg,
        backup_bgs,
        incompat,
        compat,
        ro_compat,
        uuid,
    })
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

    /// A superblock for a 16 MiB, 4 KiB-block, 2-group file system.
    fn sample() -> Vec<u8> {
        let mut sb = vec![0u8; 1024];
        let put32 = |sb: &mut Vec<u8>, at: usize, v: u32| {
            sb[at..at + 4].copy_from_slice(&v.to_le_bytes());
        };
        put32(&mut sb, 0x00, 2 * 2048); // inodes
        put32(&mut sb, 0x04, 4096); // blocks
        put32(&mut sb, 0x14, 0);
        put32(&mut sb, 0x18, 2); // 4 KiB
        put32(&mut sb, 0x20, 2048); // blocks per group
        put32(&mut sb, 0x28, 2048); // inodes per group
        sb[0x38..0x3A].copy_from_slice(&MAGIC.to_le_bytes());
        put32(&mut sb, 0x4C, 1);
        put32(&mut sb, 0x54, 11);
        sb[0x58..0x5A].copy_from_slice(&256u16.to_le_bytes());
        put32(&mut sb, 0x60, INCOMPAT_FILETYPE | INCOMPAT_EXTENTS);
        sb
    }

    const DEVICE: u64 = 4096 * 4096;

    #[test]
    fn a_good_superblock_parses() {
        let g = parse(&sample(), DEVICE).unwrap();
        assert_eq!((g.block_size, g.groups, g.inodes_count), (4096, 2, 4096));
        assert_eq!(g.inode_table_blocks(), 128);
        assert!(g.has_filetype());
    }

    #[test]
    fn every_bad_field_is_a_typed_error() {
        let ok = sample();
        let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
        let mut with = |name: &'static str, at: usize, v: u32| {
            let mut sb = ok.clone();
            sb[at..at + 4].copy_from_slice(&v.to_le_bytes());
            cases.push((name, sb));
        };
        with("log block size", 0x18, 7);
        with("zero blocks", 0x04, 0);
        with("blocks beyond device", 0x04, 5000);
        with("zero bpg", 0x20, 0);
        with("huge bpg", 0x20, 40000);
        with("zero ipg", 0x28, 0);
        with("inode count", 0x00, 5);
        with("first data", 0x14, 4096);
        with("first ino", 0x54, 3);
        with("unknown incompat", 0x60, 1 << 30);
        with("compression", 0x60, INCOMPAT_COMPRESSION);
        with("journal dev", 0x60, INCOMPAT_JOURNAL_DEV);
        for (name, sb) in cases {
            assert!(parse(&sb, DEVICE).is_err(), "{name}");
        }
        let mut sb = ok.clone();
        sb[0x58..0x5A].copy_from_slice(&200u16.to_le_bytes());
        assert!(parse(&sb, DEVICE).is_err());
        let mut sb = ok.clone();
        sb[0x38] = 0;
        assert!(matches!(parse(&sb, DEVICE), Err(Ext4Error::NotExt4)));
        assert!(parse(&ok[..100], DEVICE).is_err());
    }

    #[test]
    fn backup_superblock_groups() {
        let mut g = parse(&sample(), DEVICE).unwrap();
        assert!(g.group_has_super(7) && g.group_has_super(1) && g.group_has_super(0));
        g.ro_compat = RO_SPARSE_SUPER;
        let with: Vec<u32> = (0..60).filter(|&i| g.group_has_super(i)).collect();
        assert_eq!(with, [0, 1, 3, 5, 7, 9, 25, 27, 49]);
        g.compat = COMPAT_SPARSE_SUPER2;
        g.backup_bgs = [1, 40];
        let with: Vec<u32> = (0..60).filter(|&i| g.group_has_super(i)).collect();
        assert_eq!(with, [0, 1, 40]);
    }
}
