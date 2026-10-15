//! Property tests for the parsers: whatever the bytes are, they return
//! `None`/`Err`/some value, never panic, and what they return stays inside
//! the buffers and the device they were given.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use crate::blocks::{BlockIo, extent_runs, legacy_runs};
use crate::dir::{Event, inline_tail, walk_block};
use crate::inode::parse_inode;
use crate::sb::read_geometry;
use proptest::collection::vec;
use proptest::prelude::*;
use std::sync::atomic::AtomicU64;

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: 3000,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// A directory block built from entries whose lengths are mostly plausible,
/// so that the walk gets past its first checks.
fn plausible_block() -> impl Strategy<Value = Vec<u8>> {
    let entry = (
        any::<u32>(),
        prop_oneof![
            Just(8u16),
            Just(12),
            Just(16),
            Just(20),
            Just(24),
            any::<u16>()
        ],
        any::<u8>(),
        any::<u8>(),
        vec(any::<u8>(), 0..24),
    );
    vec(entry, 0..40).prop_map(|entries| {
        let mut b = Vec::new();
        for (inode, rec_len, name_len, ty, name) in entries {
            b.extend_from_slice(&inode.to_le_bytes());
            b.extend_from_slice(&rec_len.to_le_bytes());
            b.push(name_len % 16);
            b.push(ty);
            b.extend_from_slice(&name);
            while b.len() % 4 != 0 {
                b.push(0);
            }
        }
        b
    })
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn walking_any_bytes_stays_inside_them(
        raw in vec(any::<u8>(), 0..2048),
        plausible in plausible_block(),
        dots in any::<bool>(),
        types in any::<bool>(),
        block_size in prop_oneof![Just(1024usize), Just(4096), Just(65536)],
    ) {
        for buf in [&raw, &plausible] {
            let mut count = 0usize;
            let r = walk_block(buf, block_size, dots, types, |e| {
                count += 1;
                if let Event::Entry(d) = e {
                    assert!(!d.name.is_empty() && !d.name.contains(&b'/') && !d.name.contains(&0));
                    assert!(d.inode != 0);
                    let start = d.name.as_ptr() as usize - buf.as_ptr() as usize;
                    assert!(start + d.name.len() <= buf.len());
                }
                true
            });
            if let Err(e) = r {
                assert!(e.offset < buf.len());
            }
            // Every entry takes at least a header, so there are bounded many.
            assert!(count <= buf.len() / 8 + 1);
        }
    }

    #[test]
    fn extent_roots_and_pointers_are_checked_against_the_device(
        entries in 0u16..6,
        max in 0u16..6,
        depth in 0u16..8,
        magic_ok in prop::bool::weighted(0.9),
        body in vec(any::<u8>(), 48),
        device in vec(any::<u8>(), 1024 * 16),
        limit in 0u64..200,
    ) {
        let mut root = vec![0u8; 12];
        root[0..2].copy_from_slice(&(if magic_ok { 0xF30Au16 } else { 0 }).to_le_bytes());
        root[2..4].copy_from_slice(&entries.to_le_bytes());
        root[4..6].copy_from_slice(&max.to_le_bytes());
        root[6..8].copy_from_slice(&depth.to_le_bytes());
        root.extend_from_slice(&body);
        let blocks = 16u64;
        let budget = AtomicU64::new(blocks);
        let mut io = BlockIo::new(&device, 1024, blocks, &budget).unwrap();
        let (runs, _) = extent_runs(&mut io, &root, limit);
        let total: u64 = runs.iter().map(|r| r.len).sum();
        for r in &runs {
            assert!(r.len > 0 && r.pblk + r.len <= blocks && r.lblk < limit);
        }
        // Nothing is mapped twice over the limit, and the budget is charged.
        assert!(total <= blocks);
        let budget = AtomicU64::new(blocks);
        let mut io = BlockIo::new(&device, 1024, blocks, &budget).unwrap();
        let (runs, _) = legacy_runs(&mut io, &root[..60.min(root.len())], limit);
        for r in &runs {
            assert!(r.len > 0 && r.pblk + r.len <= blocks && r.lblk < limit);
        }
    }

    #[test]
    fn inodes_of_any_content_parse_to_something_bounded(
        raw in vec(any::<u8>(), 0..300),
        mode_ok in prop::bool::weighted(0.8),
        links in 0u16..4,
        huge in any::<bool>(),
        large_dir in any::<bool>(),
    ) {
        let mut geo = crate::testfs::geometry_4k();
        if huge { geo.ro_compat |= 0x8; }
        if large_dir { geo.incompat |= 0x4000; }
        let mut raw = raw;
        if raw.len() >= 0x30 {
            if mode_ok {
                raw[0..2].copy_from_slice(&0x81A4u16.to_le_bytes());
            }
            raw[0x1A..0x1C].copy_from_slice(&links.to_le_bytes());
        }
        if let crate::inode::Parsed::Used(rec) = parse_inode(&raw, &geo) {
            assert!(i64::try_from(rec.size).is_ok());
            assert!(rec.blocks512.checked_mul(512).is_some());
            assert!(rec.links > 0);
        }
        // The inline attribute search never reads outside the record.
        let _ = inline_tail(&raw);
    }

    #[test]
    fn superblocks_of_any_content_give_checked_geometry_or_an_error(
        body in vec(any::<u8>(), 1024),
        planted in prop::bool::weighted(0.95),
        log in 0u32..9,
        bpg in prop_oneof![Just(0u32), Just(8), Just(256), Just(8192), Just(32768), any::<u32>()],
        ipg in prop_oneof![Just(0u32), Just(8), Just(2048), any::<u32>()],
        blocks in prop_oneof![Just(0u32), Just(1), Just(4096), any::<u32>()],
        inode_size in prop_oneof![Just(0u16), Just(128), Just(256), Just(4096), any::<u16>()],
        features in any::<u32>(),
        device_blocks in 0u64..3000,
    ) {
        let mut sb = body;
        if planted {
            sb[0x38..0x3A].copy_from_slice(&0xEF53u16.to_le_bytes());
            sb[0x18..0x1C].copy_from_slice(&log.to_le_bytes());
            sb[0x20..0x24].copy_from_slice(&bpg.to_le_bytes());
            sb[0x28..0x2C].copy_from_slice(&ipg.to_le_bytes());
            sb[0x04..0x08].copy_from_slice(&blocks.to_le_bytes());
            sb[0x58..0x5A].copy_from_slice(&inode_size.to_le_bytes());
            sb[0x4C..0x50].copy_from_slice(&1u32.to_le_bytes());
            sb[0x60..0x64].copy_from_slice(&(features & 0x1FFC0).to_le_bytes());
        }
        // The device: 2 KiB of header, then however long the test wants.
        let mut device = vec![0u8; 1024];
        device.extend_from_slice(&sb);
        device.resize(usize::try_from(device_blocks).unwrap() * 1024 + 2048, 0);
        if let Ok(geo) = read_geometry(&device) {
            let device_len = device.len() as u64;
            assert!(geo.blocks_count * u64::from(geo.block_size) <= device_len);
            assert!(geo.groups > 0);
            assert_eq!(
                u64::from(geo.inodes_count),
                u64::from(geo.groups) * u64::from(geo.inodes_per_group)
            );
            assert!(geo.inode_size.is_power_of_two() && geo.inode_size <= geo.block_size);
        }
    }
}
