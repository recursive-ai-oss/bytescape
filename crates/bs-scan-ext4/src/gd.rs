//! The group descriptor table: where each block group keeps its inodes.

use crate::error::{Diagnostics, Ext4Error, ProblemKind};
use crate::le::{join64, to_usize, u16_at, u32_at};
use crate::sb::Geometry;
use crate::source::ByteSource;

const FLAG_INODE_UNINIT: u16 = 0x0001;

/// What is known about a group's inodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GroupState {
    /// The inode table can be read.
    Ok,
    /// The group has never held an inode.
    Uninit,
    /// The descriptor could not be read or describes an impossible table.
    Damaged,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Group {
    pub state: GroupState,
    /// First block of the inode table.
    pub inode_table: u64,
    /// How many inodes from the start of the table may be in use. Inodes
    /// past this are unused by the file system's own bookkeeping.
    pub used_inodes: u32,
}

const DAMAGED: Group = Group {
    state: GroupState::Damaged,
    inode_table: 0,
    used_inodes: 0,
};

/// The block that holds descriptor block number `index`.
fn descriptor_block(geo: &Geometry, index: u32) -> Option<u64> {
    if !geo.has_meta_bg() || index < geo.first_meta_bg {
        return geo
            .first_data_block
            .checked_add(u64::from(index))?
            .checked_add(1);
    }
    let per_block = geo.block_size.checked_div(geo.desc_size)?;
    let group = u64::from(index).checked_mul(u64::from(per_block))?;
    let first = group
        .checked_mul(u64::from(geo.blocks_per_group))?
        .checked_add(geo.first_data_block)?;
    let has_super = u64::from(
        u32::try_from(group)
            .map(|g| geo.group_has_super(g))
            .unwrap_or(false),
    );
    first.checked_add(has_super)
}

/// Reads every group descriptor. A descriptor block that cannot be read, or
/// a descriptor that cannot be right, marks its groups damaged and goes on.
pub(crate) fn read_groups<S: ByteSource + ?Sized>(
    src: &S,
    geo: &Geometry,
    diag: &mut Diagnostics,
) -> Result<Vec<Group>, Ext4Error> {
    let per_block = geo
        .block_size
        .checked_div(geo.desc_size)
        .filter(|&n| n > 0)
        .ok_or(Ext4Error::BadSuperblock("descriptor size"))?;
    let groups = to_usize(u64::from(geo.groups)).ok_or(Ext4Error::TooLarge("groups"))?;
    // `groups` is bounded by the device size (see `read_geometry`).
    let mut out = Vec::with_capacity(groups);
    let mut block =
        vec![0u8; to_usize(u64::from(geo.block_size)).ok_or(Ext4Error::TooLarge("block"))?];
    let table_blocks = geo.inode_table_blocks();
    let mut loaded: Option<(u32, bool)> = None;
    for group in 0..geo.groups {
        let index = group.checked_div(per_block).unwrap_or(0);
        let within = group.checked_rem(per_block).unwrap_or(0);
        let readable = match loaded {
            Some((i, ok)) if i == index => ok,
            _ => {
                let ok = descriptor_block(geo, index)
                    .filter(|&b| b < geo.blocks_count)
                    .and_then(|b| b.checked_mul(u64::from(geo.block_size)))
                    .is_some_and(|offset| src.read_at(offset, &mut block).is_ok());
                loaded = Some((index, ok));
                ok
            }
        };
        let parsed = if readable {
            let at = to_usize(u64::from(within).saturating_mul(u64::from(geo.desc_size)));
            at.and_then(|at| block.get(at..))
                .and_then(|d| parse_descriptor(geo, d, table_blocks))
        } else {
            None
        };
        match parsed {
            Some(g) => out.push(g),
            None => {
                diag.note(ProblemKind::GroupDamaged, 0, u64::from(group));
                out.push(DAMAGED);
            }
        }
    }
    Ok(out)
}

fn parse_descriptor(geo: &Geometry, d: &[u8], table_blocks: u64) -> Option<Group> {
    let table_lo = u32_at(d, 0x08)?;
    let flags = u16_at(d, 0x12)?;
    let unused_lo = u16_at(d, 0x1C)?;
    let (table_hi, unused_hi) = if geo.is_64bit() {
        (u32_at(d, 0x28)?, u16_at(d, 0x32)?)
    } else {
        (0, 0)
    };
    let inode_table = join64(table_lo, table_hi);
    let unused = u32::from(unused_lo) | u32::from(unused_hi).rotate_left(16);
    let (state, used_inodes) = if geo.has_group_flags() {
        if flags & FLAG_INODE_UNINIT != 0 {
            (GroupState::Uninit, 0)
        } else {
            (
                GroupState::Ok,
                geo.inodes_per_group
                    .saturating_sub(unused.min(geo.inodes_per_group)),
            )
        }
    } else {
        (GroupState::Ok, geo.inodes_per_group)
    };
    if state == GroupState::Uninit {
        return Some(Group {
            state,
            inode_table,
            used_inodes,
        });
    }
    // The table must lie inside the file system, after the first data block.
    let end = inode_table.checked_add(table_blocks)?;
    if inode_table < geo.first_data_block || end > geo.blocks_count {
        return None;
    }
    Some(Group {
        state,
        inode_table,
        used_inodes,
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

    #[test]
    fn short_descriptors_are_damaged_not_panics() {
        let geo = crate::testfs::geometry_4k();
        assert!(parse_descriptor(&geo, &[0u8; 10], 10).is_none());
        // A table past the end of the file system.
        let mut d = vec![0u8; 32];
        d[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_descriptor(&geo, &d, 10).is_none());
        // A table inside it.
        let mut d = vec![0u8; 32];
        d[8..12].copy_from_slice(&10u32.to_le_bytes());
        let g = parse_descriptor(&geo, &d, 10).unwrap();
        assert_eq!(g.inode_table, 10);
        assert_eq!(g.used_inodes, geo.inodes_per_group);
    }
}
