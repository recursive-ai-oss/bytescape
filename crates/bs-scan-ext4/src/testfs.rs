//! Helpers for unit tests inside the crate.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use crate::sb::Geometry;

/// A 4 KiB-block geometry with two groups.
pub(crate) fn geometry_4k() -> Geometry {
    Geometry {
        block_size: 4096,
        blocks_count: 4096,
        first_data_block: 0,
        blocks_per_group: 2048,
        inodes_per_group: 2048,
        inode_size: 256,
        groups: 2,
        inodes_count: 4096,
        first_ino: 11,
        desc_size: 32,
        first_meta_bg: 0,
        backup_bgs: [0, 0],
        incompat: 0x2 | 0x40,
        compat: 0,
        ro_compat: 0,
        uuid: [0; 16],
    }
}
